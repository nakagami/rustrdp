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
    int                  stall_count;   /* consecutive frames with no decoder output */
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
    }

    /* Helper: convert one AVFrame to malloc'd BGRA and accumulate into *result.
     * Frees any previous *result so the last decoded frame wins. */
    #define CONVERT_FRAME(frame, presult, pw, ph) do { \
        int _w = (frame)->width, _h = (frame)->height; \
        enum AVPixelFormat _fmt = map_pixfmt((frame)->format); \
        if (!d->sws || d->last_width != _w || d->last_height != _h || d->last_pix_fmt != _fmt) { \
            if (d->sws) sws_freeContext(d->sws); \
            d->sws = sws_getContext(_w, _h, _fmt, _w, _h, AV_PIX_FMT_BGRA, \
                                    SWS_BILINEAR, NULL, NULL, NULL); \
            d->last_width  = _w; d->last_height = _h; d->last_pix_fmt = _fmt; \
        } \
        if (d->sws) { \
            uint8_t *_bgra = (uint8_t*)malloc((size_t)_w * _h * 4); \
            if (_bgra) { \
                uint8_t *_dd[4] = { _bgra, NULL, NULL, NULL }; \
                int _dl[4] = { _w * 4, 0, 0, 0 }; \
                sws_scale(d->sws, \
                          (const uint8_t * const *)(frame)->data, (frame)->linesize, \
                          0, _h, _dd, _dl); \
                free(*(presult)); *(presult) = _bgra; *(pw) = _w; *(ph) = _h; \
            } \
        } \
        av_frame_unref(frame); \
    } while (0)

    uint8_t *result = NULL;
    int rw = 0, rh = 0;

    /* Drain any frames already buffered by the decoder from previous sends.
     * This is important when frame-level threading is in use: a frame decoded
     * asynchronously in a background thread may not be ready until the *next*
     * avcodec_send_packet call, so we must collect it here rather than
     * discarding it in the EAGAIN-on-send handler below. */
    for (;;) {
        int r = avcodec_receive_frame(d->ctx, d->frame);
        if (r == AVERROR(EAGAIN) || r == AVERROR_EOF) break;
        if (r < 0) break;
        CONVERT_FRAME(d->frame, &result, &rw, &rh);
    }

    d->pkt->data = (uint8_t*)(uintptr_t)data;
    d->pkt->size = len;

    int ret = avcodec_send_packet(d->ctx, d->pkt);

    /* AVERROR(EAGAIN): decoder output queue is full even after draining above.
     * This should not happen with thread_count=1, but handle defensively:
     * drain one more frame (save it, don't discard), then retry send. */
    if (ret == AVERROR(EAGAIN)) {
        int r = avcodec_receive_frame(d->ctx, d->frame);
        if (r >= 0) { CONVERT_FRAME(d->frame, &result, &rw, &rh); }
        ret = avcodec_send_packet(d->ctx, d->pkt);
    }

    if (ret < 0) {
        char errbuf[128];
        av_strerror(ret, errbuf, sizeof(errbuf));
        int pkt_idr = has_idr(data, len);
        fprintf(stderr, "[h264] avcodec_send_packet failed: %s (len=%d idr=%d)\n",
                errbuf, len, pkt_idr);
        if (!pkt_idr) {
            /* P-frame failure: do NOT flush — that would destroy the reference
             * frame buffer, causing SKIP macroblocks to decode as black.
             * Just wait for the server to send the next IDR naturally. */
            d->needs_keyframe = 1;
            free(result);
            return NULL;
        }
        /* IDR failure: flush stale state then retry. An IDR is self-contained
         * so flushing and re-sending it is always safe. */
        avcodec_flush_buffers(d->ctx);
        ret = avcodec_send_packet(d->ctx, d->pkt);
        if (ret < 0) {
            /* Flush wasn't enough — full codec reset for resolution/param change. */
            if (codec_hard_reset(d) >= 0) {
                ret = avcodec_send_packet(d->ctx, d->pkt);
            }
        }
        if (ret < 0) {
            d->needs_keyframe = 1;
            free(result);
            return NULL;
        }
    }

    /* Drain all frames produced by this packet, keep the last one */
    for (;;) {
        ret = avcodec_receive_frame(d->ctx, d->frame);
        if (ret == AVERROR(EAGAIN)) break;
        if (ret == AVERROR_EOF) break;
        if (ret < 0) break;
        CONVERT_FRAME(d->frame, &result, &rw, &rh);
    }

    #undef CONVERT_FRAME

    if (result) {
        *width = rw; *height = rh;
        d->stall_count = 0;
    } else {
        /* Decoder accepted the packet but produced no frame.  This is NORMAL
         * for H.264 streams using B-frames: the decoder may buffer 1–4 frames
         * before producing output.  Only flush after a very large number of
         * consecutive stalls, which indicates a genuine stream discontinuity. */
        d->stall_count++;
        if (d->stall_count >= 10) {
            avcodec_flush_buffers(d->ctx);
            d->needs_keyframe = 1;
            d->stall_count = 0;
        }
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
