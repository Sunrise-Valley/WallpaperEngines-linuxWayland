/* 视频 0 拷贝：进程内 VAAPI 解码 → 导出 dmabuf（供 EGL 当贴图导入）。
 *
 * 为什么这半边是 C：`AVCodecContext` / `AVHWFramesContext` 的字段布局随 ffmpeg 版本变，
 * 手抄进 Rust 既长又无法校验；放在 C 里由真头文件（无 root 提的 dev 包）保证，
 * Rust 侧只看到一个 5 函数的稳定 ABI。
 *
 * 2026-10 实测踩到的两个坑（都写进 docs/NEXT-STEPS.md 了）：
 *   1. `cc->hw_device_ctx` 和 `cc->hw_frames_ctx` **两条都要给**，只给 frames 会
 *      "Failed setup for format vaapi: hwaccel initialisation returned error" 后**静默退化成软件解码**；
 *   2. `av_hwframe_map` 之前必须把 `dst->format` 设成 `AV_PIX_FMT_DRM_PRIME`，
 *      不设的话它会按 sw_format 映射成**普通内存**，`data[0]` 是像素而不是 AVDRMFrameDescriptor
 *      （读出来是一堆像 ASCII 的垃圾值，很容易误判成"描述符布局算错了"）。
 */
#ifndef WV_AV_DECODE_H
#define WV_AV_DECODE_H

#ifdef __cplusplus
extern "C" {
#endif

/* 一帧 dmabuf 的描述：NV12 ⇒ 2 个平面（R8=Y，GR88=UV），同 fd 不同 offset/pitch/modifier */
typedef struct {
    int width, height;          /* 解码尺寸（源） */
    int nb_planes;              /* NV12 ⇒ 2（超过 2 只取前 2） */
    int fd[2];
    unsigned int fourcc[2];
    unsigned int offset[2];
    unsigned int pitch[2];
    unsigned long long modifier[2];
    int colorspace;             /* AVCOL_SPC_*（1=BT.709, 5=BT.601, 6=BT.601 变体…） */
    int color_range;            /* AVCOL_RANGE_*（1=有限范围 MPEG，2=全范围 JPEG） */
} wv_frame;

typedef struct WvDecoder WvDecoder;

/* 打开并起硬解；失败返回 NULL 并往 err 写原因 */
WvDecoder *wv_open(const char *path, char *err, int errlen);
void wv_close(WvDecoder *d);

/* 解出下一帧：1 = 拿到（out 填好）；0 = 没拿到（EOF 已回绕或需要更多包）；<0 = 出错 */
int wv_next(WvDecoder *d, wv_frame *out, char *err, int errlen);

/* 画完/swap 之后调：解映射，让解码器能复用这张 surface
 * （一定要等 GPU 真的用完再调，否则下帧会覆盖正在采样的内容） */
void wv_release(WvDecoder *d);

double wv_fps(WvDecoder *d);
int wv_hw(WvDecoder *d);        /* 1 = 真走的 VAAPI 硬解 */
const char *wv_hw_err(WvDecoder *d);  /* 没走成硬解的原因（空串 = 没出错） */

#ifdef __cplusplus
}
#endif
#endif /* WV_AV_DECODE_H */
