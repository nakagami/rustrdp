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
    int                  needs_keyframe; /* drop P-frames until next IDR/SPS */
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

    if (avcodec_open2(d->ctx, d->codec, NULL) < 0) {
        avcodec_free_context(&d->ctx);
        free(d);
        return NULL;
    }

    d->needs_keyframe = 1; /* wait for IDR before decoding anything */

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

    /* Wait for a keyframe after flush or at decoder start */
    if (d->needs_keyframe) {
        if (!has_idr(data, len)) return NULL;
        d->needs_keyframe = 0;
    }

    d->pkt->data = (uint8_t*)(uintptr_t)data;
    d->pkt->size = len;

    int ret = avcodec_send_packet(d->ctx, d->pkt);
    if (ret < 0) {
        /* Flush stale state then retry if this packet has an IDR — a keyframe
         * arriving after a resolution change or initial open can fail on the
         * first attempt because the decoder still holds undrained reference
         * frames.  After flush the same IDR almost always succeeds. */
        avcodec_flush_buffers(d->ctx);
        if (has_idr(data, len)) {
            ret = avcodec_send_packet(d->ctx, d->pkt);
            if (ret < 0) {
                /* Flush wasn't enough — a resolution/parameter change in the
                 * new SPS requires a full codec reset (free + realloc + open). */
                if (codec_hard_reset(d) >= 0) {
                    ret = avcodec_send_packet(d->ctx, d->pkt);
                }
            }
        }
        if (ret < 0) {
            d->needs_keyframe = 1;
            return NULL;
        }
    }

    /* Drain all frames buffered inside the decoder, keep the last one */
    uint8_t *result = NULL;
    for (;;) {
        ret = avcodec_receive_frame(d->ctx, d->frame);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) break;
        if (ret < 0) break;

        int w = d->frame->width;
        int h = d->frame->height;
        enum AVPixelFormat src_fmt = map_pixfmt(d->frame->format);

        /* Re-create swscale context on dimension or format change */
        if (!d->sws || d->last_width != w || d->last_height != h) {
            if (d->sws) sws_freeContext(d->sws);
            d->sws = sws_getContext(w, h, src_fmt,
                                    w, h, AV_PIX_FMT_BGRA,
                                    SWS_BILINEAR, NULL, NULL, NULL);
            d->last_width  = w;
            d->last_height = h;
        }
        if (!d->sws) { av_frame_unref(d->frame); continue; }

        uint8_t *bgra = (uint8_t*)malloc((size_t)w * h * 4);
        if (!bgra) { av_frame_unref(d->frame); continue; }

        uint8_t *dst_data[4]    = { bgra, NULL, NULL, NULL };
        int      dst_linesize[4] = { w * 4, 0, 0, 0 };

        sws_scale(d->sws,
                  (const uint8_t * const *)d->frame->data, d->frame->linesize,
                  0, h,
                  dst_data, dst_linesize);

        av_frame_unref(d->frame);

        free(result);   /* discard older buffered frame; keep latest */
        result = bgra;
        *width  = w;
        *height = h;
    }
    return result;
}

void rdp_h264_free_buf(uint8_t *buf) {
    free(buf);
}
