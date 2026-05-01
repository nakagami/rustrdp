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

/*
 * Decode one H.264 NAL packet.
 * Returns a newly malloc'd BGRA buffer (w*h*4 bytes) on success.
 * Sets *width and *height to the decoded frame dimensions.
 * Returns NULL if no frame is ready yet (EAGAIN) or on error.
 * Caller must free() the returned buffer.
 */
uint8_t* rdp_h264_decode(RdpH264Dec *d,
                          const uint8_t *data, int len,
                          int *width, int *height)
{
    if (!d || !data || len <= 0) return NULL;

    d->pkt->data = (uint8_t*)(uintptr_t)data;
    d->pkt->size = len;

    int ret = avcodec_send_packet(d->ctx, d->pkt);
    if (ret < 0) return NULL;

    ret = avcodec_receive_frame(d->ctx, d->frame);
    if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) return NULL;
    if (ret < 0) return NULL;

    int w = d->frame->width;
    int h = d->frame->height;
    *width  = w;
    *height = h;

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
    if (!d->sws) { av_frame_unref(d->frame); return NULL; }

    uint8_t *bgra = (uint8_t*)malloc((size_t)w * h * 4);
    if (!bgra) { av_frame_unref(d->frame); return NULL; }

    uint8_t       *dst_data[4]     = { bgra, NULL, NULL, NULL };
    int            dst_linesize[4] = { w * 4, 0, 0, 0 };

    sws_scale(d->sws,
              (const uint8_t * const *)d->frame->data, d->frame->linesize,
              0, h,
              dst_data, dst_linesize);

    av_frame_unref(d->frame);
    return bgra;
}

void rdp_h264_free_buf(uint8_t *buf) {
    free(buf);
}
