#include <libavcodec/avcodec.h>
#include <libavutil/imgutils.h>
#include <libswscale/swscale.h>
#include <stdlib.h>
#include <string.h>

typedef struct RdpH264Dec {
    const AVCodec       *codec;
    AVCodecContext      *ctx;
    AVFrame             *frame;
    AVPacket            *pkt;
    struct SwsContext   *sws;
    int                  last_width;
    int                  last_height;
    enum AVPixelFormat   last_pix_fmt;  /* track format changes for sws invalidation */
    int                  needs_keyframe; /* drop P-frames until next IDR/SPS */
    int                  log_frames_left;
} RdpH264Dec;

RdpH264Dec* rdp_h264_new(void) {
    RdpH264Dec *d = (RdpH264Dec*)calloc(1, sizeof(RdpH264Dec));
    if (!d) return NULL;

    d->codec = avcodec_find_decoder(AV_CODEC_ID_H264);
    if (!d->codec) { free(d); return NULL; }

    d->ctx = avcodec_alloc_context3(d->codec);
    if (!d->ctx) { free(d); return NULL; }

    /* Low-delay: RDP H.264 is in display order, no B-frame reordering needed */
    d->ctx->flags  |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
    /* Disable frame-level threading to prevent 1-frame output delay.
     * With frame threading, avcodec_receive_frame returns EAGAIN while
     * the frame is decoded in a background thread.  Our EAGAIN-on-send
     * handler discards the buffered frame, causing perpetual lag.
     * thread_count=1 keeps slice threading (intra-frame parallelism) but
     * eliminates the inter-frame pipeline delay. */
    d->ctx->thread_count = 1;

    if (avcodec_open2(d->ctx, d->codec, NULL) < 0) {
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    d->needs_keyframe = 1; /* wait for IDR before decoding anything */
    d->log_frames_left = 8;
    d->last_pix_fmt = AV_PIX_FMT_NONE;
    d->frame = av_frame_alloc();
    if (!d->frame) {
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    d->pkt = av_packet_alloc();
    if (!d->pkt) {
        av_frame_free(&d->frame);
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    return d;
}

void rdp_h264_free(RdpH264Dec *d) {
    if (!d) return;
    if (d->sws) sws_freeContext(d->sws);
    av_packet_free(&d->pkt);
    av_frame_free(&d->frame);
    avcodec_free_context(&d->ctx);
    free(d);
}

/* Map deprecated YUVJ pixel formats to their non-J equivalents. */
static enum AVPixelFormat map_pixfmt(enum AVPixelFormat fmt) {
    switch (fmt) {
    case AV_PIX_FMT_YUVJ420P: return AV_PIX_FMT_YUV420P;
    case AV_PIX_FMT_YUVJ422P: return AV_PIX_FMT_YUV422P;
    case AV_PIX_FMT_YUVJ444P: return AV_PIX_FMT_YUV444P;
    case AV_PIX_FMT_YUVJ440P: return AV_PIX_FMT_YUV440P;
    default:                  return fmt;
    }
}

#define CLAMP8(x) ((x) < 0 ? 0 : (x) > 255 ? 255 : (uint8_t)(x))

static inline void bt601_to_bgra(int y_raw, int u, int v, int full_range, uint8_t *dst)
{
    int r, g, b;
    if (full_range) {
        r = (256 * y_raw + 359 * v + 128) >> 8;
        g = (256 * y_raw -  88 * u - 183 * v + 128) >> 8;
        b = (256 * y_raw + 454 * u + 128) >> 8;
    } else {
        int c = y_raw - 16;
        r = (298 * c + 409 * v + 128) >> 8;
        g = (298 * c - 100 * u - 208 * v + 128) >> 8;
        b = (298 * c + 516 * u + 128) >> 8;
    }
    dst[0] = CLAMP8(b);
    dst[1] = CLAMP8(g);
    dst[2] = CLAMP8(r);
    dst[3] = 255;
}

static void yuv420p_to_bgra(const AVFrame *src, uint8_t *dst, int dst_stride, int full_range)
{
    for (int row = 0; row < src->height; row++) {
        const uint8_t *yrow = src->data[0] + row * src->linesize[0];
        const uint8_t *urow = src->data[1] + (row >> 1) * src->linesize[1];
        const uint8_t *vrow = src->data[2] + (row >> 1) * src->linesize[2];
        uint8_t *drow = dst + row * dst_stride;
        for (int col = 0; col < src->width; col++) {
            int u = (int)urow[col >> 1] - 128;
            int v = (int)vrow[col >> 1] - 128;
            bt601_to_bgra((int)yrow[col], u, v, full_range, drow + col * 4);
        }
    }
}

static void nv12_to_bgra(const AVFrame *src, uint8_t *dst, int dst_stride, int full_range)
{
    for (int row = 0; row < src->height; row++) {
        const uint8_t *yrow = src->data[0] + row * src->linesize[0];
        const uint8_t *uvrow = src->data[1] + (row >> 1) * src->linesize[1];
        uint8_t *drow = dst + row * dst_stride;
        for (int col = 0; col < src->width; col++) {
            int uv = (col >> 1) * 2;
            int u = (int)uvrow[uv] - 128;
            int v = (int)uvrow[uv + 1] - 128;
            bt601_to_bgra((int)yrow[col], u, v, full_range, drow + col * 4);
        }
    }
}

/* Reset codec context by freeing and reallocating with same parameters. */
static int codec_hard_reset(RdpH264Dec *d)
{
    avcodec_free_context(&d->ctx);
    if (d->sws) { sws_freeContext(d->sws); d->sws = NULL; }
    d->last_width  = 0;
    d->last_height = 0;

    d->ctx = avcodec_alloc_context3(d->codec);
    if (!d->ctx) return -1;

    d->ctx->flags  |= AV_CODEC_FLAG_LOW_DELAY;
    d->ctx->flags2 |= AV_CODEC_FLAG2_FAST;
    d->ctx->thread_count = 1;

    d->last_pix_fmt = AV_PIX_FMT_NONE;
    return avcodec_open2(d->ctx, d->codec, NULL);
}


/*
 * Scan an Annex-B H.264 bitstream for an IDR NAL (type 5) or SPS NAL (type 7).
 * Returns 1 if found, 0 otherwise.
 */
static int has_idr(const uint8_t *data, int len)
{
    int i = 0;
    while (i + 3 < len) {
        int sc_len = 0;
        if (data[i] == 0 && data[i+1] == 0) {
            if (data[i+2] == 1)
                sc_len = 3;
            else if (i + 4 < len && data[i+2] == 0 && data[i+3] == 1)
                sc_len = 4;
        }
        if (sc_len > 0) {
            int nal_start = i + sc_len;
            if (nal_start < len) {
                int nal_type = data[nal_start] & 0x1F;
                if (nal_type == 5 || nal_type == 7)
                    return 1;
            }
            i += sc_len;
        } else {
            i++;
        }
    }
    return 0;
}

/*
 * Decode one H.264 NAL packet.
 * Drops packets until an IDR/SPS is seen (after flush or at start).
 * On send failure flushes the decoder and waits for next IDR.
 * Drains all buffered output frames, returns BGRA for the last one.
 * Returns NULL when no frame is available.
 * Caller must free() the returned buffer.
 */
uint8_t* rdp_h264_decode(RdpH264Dec *d,
                          const uint8_t *data, int len,
                          int *width, int *height)
{
    if (!d || !data || len <= 0) return NULL;

    int idr = has_idr(data, len);

    /* Wait for a keyframe after flush or at decoder start */
    if (d->needs_keyframe) {
        if (!idr) return NULL;
        d->needs_keyframe = 0;
    } else if (idr) {
        /* Flush the decoder pipeline before a mid-stream IDR.
         *
         * Without this, VideoToolbox (HW decoder) buffers the IDR internally
         * and returns EAGAIN from avcodec_receive_frame for several subsequent
         * packets.  When output eventually arrives it is paired with a later
         * P-frame's AVC dirty regions instead of the IDR's full-scene regions,
         * causing permanent visual corruption (new window never redraws).
         *
         * After avcodec_flush_buffers the decoder resets cleanly, and the next
         * send_packet (this IDR) produces output in the same decode call —
         * matching the IDR's AVC regions, just like grdp's soft-reset path. */
        avcodec_flush_buffers(d->ctx);
        fprintf(stderr, "[h264] flush before IDR (len=%d)\n", len);
    }

    /* Helper: convert one AVFrame to malloc'd BGRA and accumulate into *result.
     * Frees any previous *result so the last decoded frame wins. */
    #define CONVERT_FRAME(frame, presult, pw, ph) do { \
        int _w = (frame)->width, _h = (frame)->height; \
        enum AVPixelFormat _src_fmt = (enum AVPixelFormat)(frame)->format; \
        enum AVPixelFormat _fmt = map_pixfmt(_src_fmt); \
        uint8_t *_bgra = (uint8_t*)malloc((size_t)_w * _h * 4); \
        if (_bgra) { \
            int _full_range = (_src_fmt == AV_PIX_FMT_YUVJ420P || (frame)->color_range == AVCOL_RANGE_JPEG); \
            if (_src_fmt == AV_PIX_FMT_YUV420P || _src_fmt == AV_PIX_FMT_YUVJ420P) { \
                yuv420p_to_bgra((frame), _bgra, _w * 4, _full_range); \
                free(*(presult)); *(presult) = _bgra; *(pw) = _w; *(ph) = _h; \
            } else if (_src_fmt == AV_PIX_FMT_NV12) { \
                nv12_to_bgra((frame), _bgra, _w * 4, _full_range); \
                free(*(presult)); *(presult) = _bgra; *(pw) = _w; *(ph) = _h; \
            } else { \
                if (!d->sws || d->last_width != _w || d->last_height != _h || d->last_pix_fmt != _fmt) { \
                    if (d->sws) sws_freeContext(d->sws); \
                    d->sws = sws_getContext(_w, _h, _fmt, _w, _h, AV_PIX_FMT_BGRA, \
                                            SWS_BILINEAR, NULL, NULL, NULL); \
                    d->last_width  = _w; d->last_height = _h; d->last_pix_fmt = _fmt; \
                } \
                if (d->sws) { \
                uint8_t *_dd[4] = { _bgra, NULL, NULL, NULL }; \
                int _dl[4] = { _w * 4, 0, 0, 0 }; \
                sws_scale(d->sws, \
                          (const uint8_t * const *)(frame)->data, (frame)->linesize, \
                          0, _h, _dd, _dl); \
                free(*(presult)); *(presult) = _bgra; *(pw) = _w; *(ph) = _h; \
                } else { \
                    free(_bgra); \
                } \
            } \
            if (_bgra && d->log_frames_left > 0) { \
                size_t _pixels = (size_t)_w * _h; \
                size_t _step = _pixels / 4096; \
                if (_step == 0) _step = 1; \
                unsigned long long _sum = 0, _cnt = 0; \
                for (size_t _px = 0; _px < _pixels; _px += _step) { \
                    uint8_t *_p = _bgra + _px * 4; \
                    _sum += ((unsigned long long)_p[2] * 299 + (unsigned long long)_p[1] * 587 + (unsigned long long)_p[0] * 114) / 1000; \
                    _cnt++; \
                } \
                fprintf(stderr, "[h264] frame fmt=%d range=%d full=%d size=%dx%d avg_luma=%llu samples=%llu\n", \
                        (int)_src_fmt, (int)(frame)->color_range, _full_range, _w, _h, _cnt ? _sum / _cnt : 0, _cnt); \
                d->log_frames_left--; \
            } \
        } \
        av_frame_unref(frame); \
    } while (0)

    uint8_t *result = NULL;
    int rw = 0, rh = 0;

    d->pkt->data = (uint8_t*)(uintptr_t)data;
    d->pkt->size = len;

    int ret = avcodec_send_packet(d->ctx, d->pkt);

    if (ret < 0) {
        char errbuf[128];
        av_strerror(ret, errbuf, sizeof(errbuf));
        fprintf(stderr, "[h264] avcodec_send_packet failed: %s (len=%d idr=%d)\n",
                errbuf, len, has_idr(data, len));
        avcodec_flush_buffers(d->ctx);
        d->needs_keyframe = 1;
        free(result);
        return NULL;
    }

    /* Drain all frames produced by this packet, keep the last one */
    int drain_count = 0;
    for (;;) {
        ret = avcodec_receive_frame(d->ctx, d->frame);
        if (ret == AVERROR(EAGAIN)) break;
        if (ret == AVERROR_EOF) break;
        if (ret < 0) break;
        CONVERT_FRAME(d->frame, &result, &rw, &rh);
        drain_count++;
    }

    #undef CONVERT_FRAME

    /* VideoToolbox HW stall detection:
     * Under macOS VideoToolbox, a large packet can cause EAGAIN for several
     * subsequent packets (the HW pipeline is filling up).  When the pipeline
     * drains, all buffered frames are returned in a single call — drain_count > 1.
     * The pixels we hold are from a *delayed* earlier frame, NOT the frame
     * whose AVC dirty regions we currently have.  Blitting stale pixels to the
     * wrong regions causes permanent visual corruption (windows never redraw).
     *
     * Fix: discard the stale output, flush the decoder to reset VideoToolbox,
     * and set needs_keyframe so the next IDR re-syncs cleanly.  This mirrors
     * grdp's soft-reset + keyframe-request path. */
    if (drain_count > 1) {
        fprintf(stderr, "[h264] HW stall: drained %d frames at once, flushing and requesting IDR\n", drain_count);
        avcodec_flush_buffers(d->ctx);
        d->needs_keyframe = 1;
        free(result);
        return NULL;
    }

    if (result) {
        *width = rw; *height = rh;
    }
    return result;
}

void rdp_h264_free_buf(uint8_t *buf) {
    free(buf);
}

/* Returns 1 if the decoder is waiting for an IDR keyframe, 0 otherwise. */
int rdp_h264_needs_keyframe(RdpH264Dec *d) {
    return d ? d->needs_keyframe : 1;
}
