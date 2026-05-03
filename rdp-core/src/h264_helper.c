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
    int                  seen_idr5;      /* suppress output until first true IDR (NAL type 5) */
    int                  bright_frame_seen; /* suppress until first non-trivially-dark frame */
    int                  just_flushed;   /* set after pre-IDR flush; cleared on first drain */
    int                  log_frames_left;
    int                  prev_eagain;    /* set when last packet produced EAGAIN (drain=0) */
    int                  eagain_run;     /* consecutive EAGAIN count before most recent drain */
    int                  full_blit_needed; /* set when pipeline mismatch detected; caller should use accumulated dirty regions */
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
    /* AV_CODEC_FLAG2_FAST intentionally omitted: it disables the H.264
     * in-loop deblocking filter.  The server encoder uses deblocked frames
     * as its DPB references, so our decoder must also apply deblocking to
     * keep the two DPBs in sync.  Without it, skip-coded P-frames reference
     * a stale (non-deblocked) frame and produce near-black garbled output. */
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
    fprintf(stderr, "[h264] codec hard reset: reallocate context, low_delay=1 fast=0\n");
    avcodec_free_context(&d->ctx);
    if (d->sws) { sws_freeContext(d->sws); d->sws = NULL; }
    d->last_width  = 0;
    d->last_height = 0;

    d->ctx = avcodec_alloc_context3(d->codec);
    if (!d->ctx) return -1;

    d->ctx->flags  |= AV_CODEC_FLAG_LOW_DELAY;
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
 * Scan an Annex-B H.264 bitstream for a true IDR slice (NAL type 5 only).
 * Used to suppress output of dark/zeroed frames produced by SPS-only packets
 * at decoder start.  Returns 1 if found, 0 otherwise.
 */
static int has_idr5(const uint8_t *data, int len)
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
                if (nal_type == 5)
                    return 1;
            }
            i += sc_len;
        } else {
            i++;
        }
    }
    return 0;
}

static int has_sps7(const uint8_t *data, int len)
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
                if (nal_type == 7)
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

    int idr  = has_idr(data, len);
    int idr5 = has_idr5(data, len);
    int sps7 = has_sps7(data, len);

    fprintf(stderr, "[h264] packet len=%d idr=%d idr5=%d sps7=%d jf=%d needs_keyframe=%d\n",
            len, idr, idr5, sps7, d->just_flushed, d->needs_keyframe);

    /* Mark seen_idr5 at packet-send time so that post-flush drain cycles
     * (where the IDR itself caused EAGAIN and the frame is output on the
     * next call) still correctly unlatch the IDR-5 suppression gate. */
    if (idr5) d->seen_idr5 = 1;

    /* Wait for a keyframe after flush or at decoder start */
    if (d->needs_keyframe) {
        if (!idr) return NULL;
        d->needs_keyframe = 0;
    }

    /* Pre-IDR flush: clear the decoder's DPB (decoded picture buffer) before
     * every IDR/SPS frame.  The RDP server's H.264 encoder uses multi-reference
     * frames; after a scene change (e.g. window raise) it sends a fresh IDR
     * followed immediately by a P-frame.  Without a flush that P-frame can
     * reference an old DPB entry from the pre-switch desktop and produce
     * old-desktop pixels in the new window region.  After flushing, the only
     * DPB entry is the IDR frame itself, so subsequent P-frames reference the
     * correct (new) content.
     *
     * Side effect: the first avcodec_receive_frame call after flush often
     * returns EAGAIN (decoder needs one extra packet before outputting).  We
     * handle this with the just_flushed flag below: a drain_count > 1 on the
     * very next packet is expected (IDR + following frame both drain at once)
     * and should NOT trigger the stale-output flush path. */
    if (idr) {
        fprintf(stderr, "[h264] pre-IDR/SPS flush: len=%d idr5=%d sps7=%d\n", len, idr5, sps7);
        avcodec_flush_buffers(d->ctx);
        d->just_flushed      = 1;
        d->log_frames_left   = 8;  /* re-enable per-frame luma logging after each IDR */
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
            if (_bgra) { \
                size_t _pixels = (size_t)_w * _h; \
                size_t _step = _pixels / 4096; \
                if (_step == 0) _step = 1; \
                unsigned long long _sum = 0, _cnt = 0; \
                for (size_t _px = 0; _px < _pixels; _px += _step) { \
                    uint8_t *_p = _bgra + _px * 4; \
                    _sum += ((unsigned long long)_p[2] * 299 + (unsigned long long)_p[1] * 587 + (unsigned long long)_p[0] * 114) / 1000; \
                    _cnt++; \
                } \
                fprintf(stderr, "[h264] frame pict=%d fmt=%d range=%d full=%d size=%dx%d avg_luma=%llu samples=%llu px0=(%d,%d,%d)\n", \
                        (int)(frame)->pict_type, (int)_src_fmt, (int)(frame)->color_range, _full_range, _w, _h, \
                        _cnt ? _sum / _cnt : 0, _cnt, \
                        (int)_bgra[2], (int)_bgra[1], (int)_bgra[0]); \
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
        d->needs_keyframe    = 1;
        d->seen_idr5         = 0;
        d->bright_frame_seen = 0;
        d->just_flushed      = 0;
        free(result);
        return NULL;
    }

    /* Drain all frames produced by this packet, keep the last one */
    int drain_count = 0;
    int last_pict_type = AV_PICTURE_TYPE_NONE;
    for (;;) {
        ret = avcodec_receive_frame(d->ctx, d->frame);
        if (ret == AVERROR(EAGAIN)) break;
        if (ret == AVERROR_EOF) break;
        if (ret < 0) break;
        last_pict_type = d->frame->pict_type;  /* save before CONVERT_FRAME unrefs */
        CONVERT_FRAME(d->frame, &result, &rw, &rh);
        drain_count++;
    }

    if (drain_count == 0) {
        /* SW decoder returned EAGAIN despite LOW_DELAY — the decoder has a
         * one-frame internal delay for this packet.  Track this so the next
         * call can detect a dirty-region mismatch. */
        fprintf(stderr, "[h264] decoder EAGAIN (drain=0, len=%d idr=%d)\n", len, idr);
        d->prev_eagain = 1;
        d->eagain_run++;
        return NULL;
    }

    /* Pipeline-mismatch detection.
     *
     * When the SW decoder has an internal pipeline delay (consecutive EAGAIN
     * returns from multi-slice frames), the frame that finally drains covers
     * the area described by ALL the earlier EAGAIN packets' dirty regions,
     * not just the current packet's region.
     *
     * Signal full_blit_needed so the Rust layer uses its accumulated dirty
     * region union (collected across all EAGAIN packets) for this blit.
     *
     * Exception: just_flushed=1 means a pre-IDR flush caused the EAGAIN; that
     * is an expected one-frame lag handled separately below.
     *
     * Note: we do NOT signal full_blit_needed for the subsequent P-frames that
     * follow the mismatch frame — those frames have their own correct dirty
     * regions and correct decoded content. Forcing a full-blit on them would
     * write stale DPB content (black corners / old desktop) outside the dirty
     * region, causing the observed distortion. */
    int was_mismatch = (d->prev_eagain && drain_count == 1 && !d->just_flushed);

    if (was_mismatch) {
        fprintf(stderr, "[h264] pipeline mismatch (prev EAGAIN + drain=1, len=%d eagain_run=%d drain=%d): use accumulated regions\n",
                len, d->eagain_run, drain_count);
        d->full_blit_needed = 1;
        d->log_frames_left = 8;
    }
    d->prev_eagain = 0;
    d->eagain_run  = 0;

    /* Stale-output detection: drain_count > 1 means the SW decoder output
     * multiple buffered frames in one call.
     *
     * Exception: when just_flushed=1 a drain of 2 is EXPECTED — the pre-IDR
     * flush caused EAGAIN on the IDR send, so the IDR frame is buffered until
     * the next packet arrives.  The last drained frame is the correct one to
     * display; keep it and clear just_flushed.
     *
     * Also exempt: was_mismatch — we are already at the first drain after a
     * pipeline delay; multi-drain is not unexpected here.
     *
     * Otherwise drain_count > 1 is a genuine pipeline stall: the pixels belong
     * to an earlier frame and would be blitted with the CURRENT packet's dirty
     * regions, causing corruption.  Discard the output and request a fresh IDR. */
    if (drain_count > 1) {
        if (d->just_flushed || was_mismatch) {
            fprintf(stderr, "[h264] post-flush/mismatch drain=%d, showing last frame\n", drain_count);
            d->just_flushed = 0;
        } else {
            fprintf(stderr, "[h264] stale-output: drained %d frames at once, flushing and requesting IDR\n", drain_count);
            avcodec_flush_buffers(d->ctx);
            d->needs_keyframe    = 1;
            d->seen_idr5         = 0;
            d->bright_frame_seen = 0;
            d->just_flushed      = 0;
            free(result);
            return NULL;
        }
    }

    /* Clear just_flushed on the first successful drain after a pre-IDR flush. */
    if (drain_count == 1) d->just_flushed = 0;

    /* Non-IDR I-frame handling: flush DPB and re-decode.
     *
     * The RDP server's H.264 encoder sends "scene change" keyframes as plain
     * I-slices (NAL type 1, slice_type=I) rather than IDR slices (NAL type 5).
     * These are not detected by has_idr() so the pre-send flush above does not
     * fire.  After decoding such a frame the DPB still contains old reference
     * frames from before the scene change.  Subsequent P-frames can use those
     * old entries as a reference (via explicit ref_idx in the slice header),
     * producing old-desktop pixels in the newly-changed region — the classic
     * "window appears briefly then reverts" bug.
     *
     * Fix: when the decoded frame is an I-frame (IDR or non-IDR), flush the
     * DPB and immediately re-decode the same packet.  After the re-decode the
     * DPB contains ONLY this I-frame, so any subsequent P-frame must reference
     * it regardless of ref_idx.  The I-frame is self-contained, so the pixel
     * output is identical whether or not the DPB was flushed.
     *
     * We skip this step for true IDR frames (idr==1) because the pre-send
     * flush above already handled them; re-flushing would double-work and might
     * cause an extra EAGAIN. */
    if (!idr && drain_count == 1 && result != NULL &&
            last_pict_type == AV_PICTURE_TYPE_I) {
        fprintf(stderr, "[h264] non-IDR I-frame (len=%d): flush DPB + re-decode\n", len);
        avcodec_flush_buffers(d->ctx);
        d->log_frames_left = 8;
        d->pkt->data = (uint8_t*)(uintptr_t)data;
        d->pkt->size = len;
        if (avcodec_send_packet(d->ctx, d->pkt) == 0) {
            int ret_re = avcodec_receive_frame(d->ctx, d->frame);
            if (ret_re == 0) {
                last_pict_type = d->frame->pict_type;
                CONVERT_FRAME(d->frame, &result, &rw, &rh);
                /* Re-decode succeeded immediately; no EAGAIN pending. */
            } else if (ret_re == AVERROR(EAGAIN)) {
                /* Decoder needs one more packet before outputting — the
                 * re-decoded I-frame will drain together with the next P-frame
                 * (drain_count=2).  Set just_flushed so that the stale-output
                 * detection treats it as an expected post-flush drain. */
                d->just_flushed = 1;
                fprintf(stderr, "[h264] non-IDR I-frame re-decode EAGAIN, just_flushed=1\n");
            }
        }
    }

    #undef CONVERT_FRAME

    if (result) {
        /* Suppress output until the first true IDR (NAL type 5) has been sent.
         * seen_idr5 is set at packet-receipt time (above) so it is already
         * correct here even when the IDR itself caused EAGAIN. */
        if (!d->seen_idr5) {
            free(result);
            return NULL;
        }

        /* Suppress display until the first non-trivially-dark frame.
         * RDP servers often send all-black IDR frames during encoder
         * initialisation before the actual desktop content is ready.
         * VideoToolbox (HW decoder) naturally stalls on these and never
         * outputs them; to match that behaviour we suppress frames whose
         * average luma is below a "definitely black" threshold until we have
         * seen at least one bright frame.  Once latched, all frames pass
         * through — including legitimately dark content. */
        if (!d->bright_frame_seen) {
            /* Compute approximate average luma from the BGRA buffer. */
            size_t _pixels = (size_t)rw * rh;
            size_t _step = _pixels / 1024;
            if (_step == 0) _step = 1;
            unsigned long long _sum = 0, _cnt = 0;
            for (size_t _px = 0; _px < _pixels; _px += _step) {
                uint8_t *_p = result + _px * 4;
                _sum += ((unsigned long long)_p[2] * 299 +
                         (unsigned long long)_p[1] * 587 +
                         (unsigned long long)_p[0] * 114) / 1000;
                _cnt++;
            }
            unsigned long long _avg = _cnt ? _sum / _cnt : 0;
            /* Only suppress truly-black encoder-init frames (luma ≤ 3).
             * The server typically sends one all-black IDR (luma≈0) during
             * encoder initialisation.  After that the actual desktop content
             * starts immediately — even with a dark wallpaper (luma≈9-15).
             * The old threshold of 20 mistakenly suppressed dark-wallpaper
             * desktops forever, producing a permanent black screen. */
            if (_avg <= 3) {
                fprintf(stderr, "[h264] suppress initial dark frame (avg_luma=%llu)\n", _avg);
                free(result);
                return NULL;
            }
            d->bright_frame_seen = 1;
            fprintf(stderr, "[h264] first bright frame (avg_luma=%llu), display unlocked\n", _avg);
        }

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

/* Returns 1 if the last decoded frame must be blitted to the full surface
 * (ignoring AVC dirty regions) due to a pipeline mismatch.  Clears the flag. */
int rdp_h264_take_full_blit(RdpH264Dec *d) {
    if (!d) return 0;
    int v = d->full_blit_needed;
    d->full_blit_needed = 0;
    return v;
}
