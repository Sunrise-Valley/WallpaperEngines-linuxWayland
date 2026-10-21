/* 见 av_decode.h 的说明。 */
#include "av_decode.h"

#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/pixfmt.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

struct WvDecoder {
    AVFormatContext *fmt;
    AVCodecContext *cc;
    AVBufferRef *device;
    AVBufferRef *frames;
    AVPacket *pkt;
    AVFrame *frame;     /* 解码出来的（VAAPI） */
    AVFrame *mapped;    /* map 成 DRM_PRIME 的 */
    int video_stream;
    int hw;
    int eof;            /* 需要 seek 回绕 */
    char hw_err[200];   /* 没走成硬解的原因（诊断用） */
};

static void set_err(char *err, int errlen, const char *what, int code) {
    if (!err || errlen <= 0) return;
    char e[160] = {0};
    if (code < 0) av_strerror(code, e, sizeof(e));
    snprintf(err, errlen, "%s%s%s", what, code < 0 ? ": " : "", e);
}

static enum AVPixelFormat get_format_vaapi(AVCodecContext *ctx, const enum AVPixelFormat *fmts) {
    (void)ctx;
    for (const enum AVPixelFormat *p = fmts; *p != AV_PIX_FMT_NONE; p++) {
        if (*p == AV_PIX_FMT_VAAPI) return *p;
    }
    return AV_PIX_FMT_NONE;
}

WvDecoder *wv_open(const char *path, char *err, int errlen) {
    av_log_set_level(AV_LOG_ERROR);
    WvDecoder *d = calloc(1, sizeof(*d));
    if (!d) return NULL;

    int r = avformat_open_input(&d->fmt, path, NULL, NULL);
    if (r < 0) { set_err(err, errlen, "avformat_open_input", r); goto fail; }
    r = avformat_find_stream_info(d->fmt, NULL);
    if (r < 0) { set_err(err, errlen, "find_stream_info", r); goto fail; }
    d->video_stream = av_find_best_stream(d->fmt, AVMEDIA_TYPE_VIDEO, -1, -1, NULL, 0);
    if (d->video_stream < 0) { set_err(err, errlen, "没有视频流", 0); goto fail; }

    const AVCodec *dec = avcodec_find_decoder(d->fmt->streams[d->video_stream]->codecpar->codec_id);
    if (!dec) { set_err(err, errlen, "没有对应的解码器", 0); goto fail; }
    d->cc = avcodec_alloc_context3(dec);
    if (!d->cc) { set_err(err, errlen, "avcodec_alloc_context3 失败", 0); goto fail; }
    avcodec_parameters_to_context(d->cc, d->fmt->streams[d->video_stream]->codecpar);

    /* ① 进程内 VAAPI 设备（拿不到就走软件，hw 标记为 0） */
    r = av_hwdevice_ctx_create(&d->device, AV_HWDEVICE_TYPE_VAAPI, "/dev/dri/renderD128", NULL, 0);
    if (r == 0) {
        d->frames = av_hwframe_ctx_alloc(d->device);
        if (d->frames) {
            AVHWFramesContext *fc = (AVHWFramesContext *)d->frames->data;
            fc->format = AV_PIX_FMT_VAAPI;
            fc->sw_format = AV_PIX_FMT_NV12;
            fc->width = d->cc->width;
            fc->height = d->cc->height;
            if (av_hwframe_ctx_init(d->frames) >= 0) {
                /* ② 两条都要给：只给 frames 会静默退化成软件解码 */
                d->cc->hw_device_ctx = av_buffer_ref(d->device);
                d->cc->hw_frames_ctx = av_buffer_ref(d->frames);
                d->cc->get_format = get_format_vaapi;
            } else {
                snprintf(d->hw_err, sizeof(d->hw_err), "av_hwframe_ctx_init 失败（NV12 %dx%d）",
                         d->cc->width, d->cc->height);
                av_buffer_unref(&d->frames);
            }
        } else {
            snprintf(d->hw_err, sizeof(d->hw_err), "av_hwframe_ctx_alloc 失败");
        }
    } else {
        char e[128] = {0};
        av_strerror(r, e, sizeof(e));
        snprintf(d->hw_err, sizeof(d->hw_err), "av_hwdevice_ctx_create(VAAPI /dev/dri/renderD128): %s", e);
    }

    r = avcodec_open2(d->cc, dec, NULL);
    if (r < 0) { set_err(err, errlen, "avcodec_open2", r); goto fail; }

    d->pkt = av_packet_alloc();
    d->frame = av_frame_alloc();
    d->mapped = av_frame_alloc();
    if (!d->pkt || !d->frame || !d->mapped) { set_err(err, errlen, "alloc packet/frame 失败", 0); goto fail; }
    return d;

fail:
    wv_close(d);
    return NULL;
}

void wv_close(WvDecoder *d) {
    if (!d) return;
    wv_release(d);
    if (d->pkt) av_packet_free(&d->pkt);
    if (d->frame) av_frame_free(&d->frame);
    if (d->mapped) av_frame_free(&d->mapped);
    if (d->cc) avcodec_free_context(&d->cc);
    if (d->frames) av_buffer_unref(&d->frames);
    if (d->device) av_buffer_unref(&d->device);
    if (d->fmt) avformat_close_input(&d->fmt);
    free(d);
}

void wv_release(WvDecoder *d) {
    /* 这个 ffmpeg 版本没有 av_hwframe_unmap —— 解映射就是用 av_frame_unref
     * 把 mapped 帧放掉（它持着原 VA surface 的引用，放掉之后解码器才能复用那张 surface）。 */
    if (d && d->mapped && d->mapped->data[0]) av_frame_unref(d->mapped);
}

/* 把 out 填好；失败返回 <0 */
static int fill(wv_frame *out, AVFrame *mapped, AVCodecContext *cc, AVFrame *src) {
    AVDRMFrameDescriptor *desc = (AVDRMFrameDescriptor *)mapped->data[0];
    if (!desc || desc->nb_layers < 1) return -1;
    out->width = mapped->width;
    out->height = mapped->height;
    out->nb_planes = 0;
    for (int l = 0; l < desc->nb_layers && out->nb_planes < 2; l++) {
        AVDRMLayerDescriptor *L = &desc->layers[l];
        if (L->nb_planes < 1) continue;
        AVDRMPlaneDescriptor *P = &L->planes[0];
        AVDRMObjectDescriptor *O = &desc->objects[P->object_index];
        int i = out->nb_planes++;
        out->fd[i] = O->fd;
        out->fourcc[i] = L->format;
        out->offset[i] = (unsigned int)P->offset;
        out->pitch[i] = (unsigned int)P->pitch;
        out->modifier[i] = O->format_modifier;
    }
    out->colorspace = src->colorspace != AVCOL_SPC_UNSPECIFIED ? src->colorspace : cc->colorspace;
    out->color_range = src->color_range != AVCOL_RANGE_UNSPECIFIED ? src->color_range : cc->color_range;
    return out->nb_planes > 0 ? 0 : -1;
}

int wv_next(WvDecoder *d, wv_frame *out, char *err, int errlen) {
    if (!d || !out) return -1;
    wv_release(d); /* 上一帧用完才解下一帧（调用方保证画完/swap 之后才调本函数） */

    for (int guard = 0; guard < 200; guard++) {
        if (d->eof) {
            /* 壁纸要无限循环：EOF 就回到开头（和旧路 ffmpeg -stream_loop -1 等价） */
            av_seek_frame(d->fmt, d->video_stream, 0, AVSEEK_FLAG_BACKWARD);
            avcodec_flush_buffers(d->cc);
            d->eof = 0;
        }
        int r = av_read_frame(d->fmt, d->pkt);
        if (r < 0) { d->eof = 1; continue; }
        if (d->pkt->stream_index != d->video_stream) { av_packet_unref(d->pkt); continue; }

        r = avcodec_send_packet(d->cc, d->pkt);
        av_packet_unref(d->pkt);
        if (r < 0 && r != AVERROR(EAGAIN)) { set_err(err, errlen, "send_packet", r); return -1; }

        while (1) {
            r = avcodec_receive_frame(d->cc, d->frame);
            if (r == AVERROR(EAGAIN)) break;
            if (r == AVERROR_EOF) { d->eof = 1; break; }
            if (r < 0) { set_err(err, errlen, "receive_frame", r); return -1; }
            d->hw = d->frame->format == AV_PIX_FMT_VAAPI;
            /* ★ dst->format 必须先设：不设就映射成普通内存（见头文件注释） */
            d->mapped->format = AV_PIX_FMT_DRM_PRIME;
            r = av_hwframe_map(d->mapped, d->frame, AV_HWFRAME_MAP_READ);
            av_frame_unref(d->frame);
            if (r < 0) { set_err(err, errlen, "hwframe_map(DRM_PRIME)", r); return -1; }
            if (fill(out, d->mapped, d->cc, d->frame) == 0) return 1;
        }
    }
    set_err(err, errlen, "连续 200 轮没解出帧", 0);
    return -1;
}

double wv_fps(WvDecoder *d) {
    if (!d) return 0.0;
    AVRational fr = d->fmt->streams[d->video_stream]->avg_frame_rate;
    return fr.num > 0 && fr.den > 0 ? av_q2d(fr) : 0.0;
}

int wv_hw(WvDecoder *d) { return d ? d->hw : 0; }

const char *wv_hw_err(WvDecoder *d) { return d ? d->hw_err : ""; }
