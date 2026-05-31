#include "libavcodec/avcodec.h"
#include "libavcodec/defs.h"
#include "libavformat/avformat.h"
#include "libavutil/avstring.h"
#include "libavutil/channel_layout.h"
#include "libavutil/error.h"
#include "libavutil/frame.h"
#include "libavutil/log.h"
#include "libavutil/mem.h"
#include "libavutil/samplefmt.h"
#include "libswresample/swresample.h"

#include <stdint.h>
#include <stdio.h>
#include <string.h>

enum {
    SCRYER_CODEC_AC3 = 0,
    SCRYER_CODEC_EAC3 = 1,
    SCRYER_CODEC_DTS = 2,
    SCRYER_CODEC_TRUEHD = 3,
};

enum {
    SCRYER_SYNC_CODEC_AC3 = 0,
    SCRYER_SYNC_CODEC_EAC3 = 1,
    SCRYER_SYNC_CODEC_DTS = 2,
    SCRYER_SYNC_CODEC_DTS_HD_MA_CORE = 3,
    SCRYER_SYNC_CODEC_TRUEHD = 4,
};

enum {
    SCRYER_FFMPEG_DECODED = 0,
    SCRYER_FFMPEG_UNSUPPORTED = 1,
    SCRYER_FFMPEG_ERROR = 2,
};

typedef struct ScryerFfmpegDecodeResult {
    int32_t status_code;
    uint32_t sample_rate_hz;
    uint16_t channels;
    uint64_t samples_decoded;
    uint8_t *pcm_f32le;
    uintptr_t pcm_f32le_len;
    char message[256];
} ScryerFfmpegDecodeResult;

typedef int32_t (*ScryerFfmpegPcmCallback)(void *userdata, const int16_t *samples,
                                           uintptr_t sample_count,
                                           uint32_t sample_rate_hz,
                                           uint16_t channels);

typedef struct ScryerFfmpegSyncDecodeResult {
    int32_t status_code;
    uint32_t stream_index;
    uint32_t codec;
    uint32_t sample_rate_hz;
    uint16_t channels;
    uint64_t samples_decoded;
    int64_t duration_ms;
    int64_t timeline_start_ms;
    int32_t used_core_fallback;
    char source_codec_name[64];
    char source_profile[64];
    char language[32];
    char message[256];
    char warnings[512];
} ScryerFfmpegSyncDecodeResult;

static enum AVCodecID codec_id_for_scryer(uint32_t codec)
{
    switch (codec) {
    case SCRYER_CODEC_AC3:
        return AV_CODEC_ID_AC3;
    case SCRYER_CODEC_EAC3:
        return AV_CODEC_ID_EAC3;
    case SCRYER_CODEC_DTS:
        return AV_CODEC_ID_DTS;
    case SCRYER_CODEC_TRUEHD:
        return AV_CODEC_ID_TRUEHD;
    default:
        return AV_CODEC_ID_NONE;
    }
}

static void set_message(ScryerFfmpegDecodeResult *out, const char *message)
{
    snprintf(out->message, sizeof(out->message), "%s", message);
}

static int set_error(ScryerFfmpegDecodeResult *out, const char *message)
{
    out->status_code = SCRYER_FFMPEG_ERROR;
    set_message(out, message);
    return SCRYER_FFMPEG_ERROR;
}

static int set_av_error(ScryerFfmpegDecodeResult *out, const char *prefix, int error)
{
    char detail[128] = {0};
    av_strerror(error, detail, sizeof(detail));
    snprintf(out->message, sizeof(out->message), "%s: %s", prefix, detail);
    out->status_code = SCRYER_FFMPEG_ERROR;
    return SCRYER_FFMPEG_ERROR;
}

static float read_sample(const uint8_t *data, enum AVSampleFormat format, int index)
{
    switch (format) {
    case AV_SAMPLE_FMT_U8:
        return (((const uint8_t *)data)[index] - 128) / 128.0f;
    case AV_SAMPLE_FMT_S16:
        return ((const int16_t *)data)[index] / 32768.0f;
    case AV_SAMPLE_FMT_S32:
        return ((const int32_t *)data)[index] / 2147483648.0f;
    case AV_SAMPLE_FMT_FLT:
        return ((const float *)data)[index];
    case AV_SAMPLE_FMT_DBL:
        return (float)((const double *)data)[index];
    case AV_SAMPLE_FMT_S64:
        return (float)(((const int64_t *)data)[index] / 9223372036854775808.0);
    default:
        return 0.0f;
    }
}

static int append_frame(ScryerFfmpegDecodeResult *out, const AVFrame *frame, int mixdown_mono)
{
    const int source_channels = frame->ch_layout.nb_channels;
    const int output_channels = mixdown_mono ? 1 : source_channels;
    const int planar = av_sample_fmt_is_planar(frame->format);
    const enum AVSampleFormat packed_format = av_get_packed_sample_fmt(frame->format);

    if (source_channels <= 0 || frame->nb_samples <= 0) {
        return 0;
    }
    if (output_channels > UINT16_MAX) {
        return set_error(out, "FFmpeg returned too many channels");
    }
    if (packed_format == AV_SAMPLE_FMT_NONE) {
        return set_error(out, "FFmpeg returned an unsupported sample format");
    }
    if (out->sample_rate_hz != 0 && out->sample_rate_hz != (uint32_t)frame->sample_rate) {
        return set_error(out, "FFmpeg sample rate changed during the decode window");
    }
    if (out->channels != 0 && out->channels != (uint16_t)output_channels) {
        return set_error(out, "FFmpeg channel count changed during the decode window");
    }

    const uintptr_t old_len = out->pcm_f32le_len;
    const uintptr_t frame_values = (uintptr_t)frame->nb_samples * (uintptr_t)output_channels;
    const uintptr_t frame_bytes = frame_values * sizeof(float);
    uint8_t *next = av_realloc(out->pcm_f32le, old_len + frame_bytes);
    if (!next) {
        return set_error(out, "failed to grow FFmpeg PCM output buffer");
    }

    out->pcm_f32le = next;
    out->pcm_f32le_len = old_len + frame_bytes;
    out->sample_rate_hz = (uint32_t)frame->sample_rate;
    out->channels = (uint16_t)output_channels;
    out->samples_decoded += (uint64_t)frame->nb_samples;

    float *dst = (float *)(void *)(out->pcm_f32le + old_len);
    for (int sample = 0; sample < frame->nb_samples; sample++) {
        if (mixdown_mono) {
            float sum = 0.0f;
            for (int channel = 0; channel < source_channels; channel++) {
                const uint8_t *plane = planar ? frame->extended_data[channel] : frame->extended_data[0];
                const int index = planar ? sample : sample * source_channels + channel;
                sum += read_sample(plane, packed_format, index);
            }
            *dst++ = sum / (float)source_channels;
        } else {
            for (int channel = 0; channel < source_channels; channel++) {
                const uint8_t *plane = planar ? frame->extended_data[channel] : frame->extended_data[0];
                const int index = planar ? sample : sample * source_channels + channel;
                *dst++ = read_sample(plane, packed_format, index);
            }
        }
    }

    return SCRYER_FFMPEG_DECODED;
}

static int receive_frames(AVCodecContext *context, AVFrame *frame, int mixdown_mono,
                          ScryerFfmpegDecodeResult *out)
{
    for (;;) {
        int ret = avcodec_receive_frame(context, frame);
        if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
            return SCRYER_FFMPEG_DECODED;
        }
        if (ret < 0) {
            return set_av_error(out, "FFmpeg failed while receiving decoded PCM", ret);
        }
        ret = append_frame(out, frame, mixdown_mono);
        av_frame_unref(frame);
        if (ret != SCRYER_FFMPEG_DECODED) {
            return ret;
        }
    }
}

int32_t scryer_ffmpeg_decode_window(uint32_t codec, const uint8_t *const *packet_data,
                                    const uintptr_t *packet_lens, const int64_t *pts_ms,
                                    uintptr_t packet_count, int32_t mixdown_mono,
                                    ScryerFfmpegDecodeResult *out)
{
    av_log_set_level(AV_LOG_QUIET);
    memset(out, 0, sizeof(*out));
    out->status_code = SCRYER_FFMPEG_ERROR;

    const enum AVCodecID av_codec_id = codec_id_for_scryer(codec);
    if (av_codec_id == AV_CODEC_ID_NONE) {
        out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
        set_message(out, "unsupported Scryer codec id");
        return out->status_code;
    }

    const AVCodec *decoder = avcodec_find_decoder(av_codec_id);
    if (!decoder) {
        out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
        set_message(out, "vendored FFmpeg decoder is not enabled");
        return out->status_code;
    }

    AVCodecContext *context = avcodec_alloc_context3(decoder);
    AVPacket *packet = av_packet_alloc();
    AVFrame *frame = av_frame_alloc();
    if (!context || !packet || !frame) {
        avcodec_free_context(&context);
        av_packet_free(&packet);
        av_frame_free(&frame);
        return set_error(out, "failed to allocate FFmpeg decoder state");
    }

    context->pkt_timebase.num = 1;
    context->pkt_timebase.den = 1000;

    int ret = avcodec_open2(context, decoder, NULL);
    if (ret < 0) {
        avcodec_free_context(&context);
        av_packet_free(&packet);
        av_frame_free(&frame);
        return set_av_error(out, "failed to open FFmpeg decoder", ret);
    }

    for (uintptr_t i = 0; i < packet_count; i++) {
        if (!packet_data[i] || packet_lens[i] == 0) {
            continue;
        }

        ret = av_new_packet(packet, (int)packet_lens[i]);
        if (ret < 0) {
            avcodec_free_context(&context);
            av_packet_free(&packet);
            av_frame_free(&frame);
            return set_av_error(out, "failed to allocate FFmpeg packet", ret);
        }
        memcpy(packet->data, packet_data[i], packet_lens[i]);
        if (pts_ms) {
            packet->pts = pts_ms[i];
            packet->dts = pts_ms[i];
        }

        ret = avcodec_send_packet(context, packet);
        av_packet_unref(packet);
        if (ret < 0) {
            avcodec_free_context(&context);
            av_packet_free(&packet);
            av_frame_free(&frame);
            return set_av_error(out, "FFmpeg failed while sending an audio packet", ret);
        }

        ret = receive_frames(context, frame, mixdown_mono != 0, out);
        if (ret != SCRYER_FFMPEG_DECODED) {
            avcodec_free_context(&context);
            av_packet_free(&packet);
            av_frame_free(&frame);
            return ret;
        }
    }

    ret = avcodec_send_packet(context, NULL);
    if (ret >= 0) {
        ret = receive_frames(context, frame, mixdown_mono != 0, out);
    }

    avcodec_free_context(&context);
    av_packet_free(&packet);
    av_frame_free(&frame);

    if (ret != SCRYER_FFMPEG_DECODED) {
        return ret;
    }
    if (out->samples_decoded == 0) {
        return set_error(out, "FFmpeg decoder produced no PCM samples");
    }

    out->status_code = SCRYER_FFMPEG_DECODED;
    set_message(out, "decoded by vendored FFmpeg");
    return out->status_code;
}

void scryer_ffmpeg_free(void *ptr)
{
    av_free(ptr);
}

static void set_sync_decode_message(ScryerFfmpegSyncDecodeResult *out, const char *message)
{
    snprintf(out->message, sizeof(out->message), "%s", message);
}

static int set_sync_decode_error(ScryerFfmpegSyncDecodeResult *out, const char *message)
{
    out->status_code = SCRYER_FFMPEG_ERROR;
    set_sync_decode_message(out, message);
    return SCRYER_FFMPEG_ERROR;
}

static int set_sync_decode_av_error(ScryerFfmpegSyncDecodeResult *out, const char *prefix, int error)
{
    char detail[128] = {0};
    av_strerror(error, detail, sizeof(detail));
    snprintf(out->message, sizeof(out->message), "%s: %s", prefix, detail);
    out->status_code = SCRYER_FFMPEG_ERROR;
    return SCRYER_FFMPEG_ERROR;
}

static enum AVCodecID codec_id_for_sync_decode(uint32_t codec)
{
    switch (codec) {
    case SCRYER_SYNC_CODEC_AC3:
        return AV_CODEC_ID_AC3;
    case SCRYER_SYNC_CODEC_EAC3:
        return AV_CODEC_ID_EAC3;
    case SCRYER_SYNC_CODEC_DTS:
    case SCRYER_SYNC_CODEC_DTS_HD_MA_CORE:
        return AV_CODEC_ID_DTS;
    case SCRYER_SYNC_CODEC_TRUEHD:
        return AV_CODEC_ID_TRUEHD;
    default:
        return AV_CODEC_ID_NONE;
    }
}

static uint32_t sync_decode_codec_for_av(enum AVCodecID codec_id, int dts_core)
{
    switch (codec_id) {
    case AV_CODEC_ID_AC3:
        return SCRYER_SYNC_CODEC_AC3;
    case AV_CODEC_ID_EAC3:
        return SCRYER_SYNC_CODEC_EAC3;
    case AV_CODEC_ID_DTS:
        return dts_core ? SCRYER_SYNC_CODEC_DTS_HD_MA_CORE : SCRYER_SYNC_CODEC_DTS;
    case AV_CODEC_ID_TRUEHD:
    case AV_CODEC_ID_MLP:
        return SCRYER_SYNC_CODEC_TRUEHD;
    default:
        return UINT32_MAX;
    }
}

static int codec_is_targeted(enum AVCodecID codec_id)
{
    return codec_id == AV_CODEC_ID_AC3 || codec_id == AV_CODEC_ID_EAC3 ||
           codec_id == AV_CODEC_ID_DTS || codec_id == AV_CODEC_ID_TRUEHD ||
           codec_id == AV_CODEC_ID_MLP;
}

static int stream_matches_language(const AVStream *stream, const char *language)
{
    if (!language || !language[0]) {
        return 0;
    }
    const AVDictionaryEntry *tag = av_dict_get(stream->metadata, "language", NULL, 0);
    if (!tag || !tag->value) {
        return 0;
    }
    return av_strcasecmp(tag->value, language) == 0 ||
           av_strncasecmp(tag->value, language, 2) == 0;
}

static int select_audio_stream(AVFormatContext *format, int requested_stream_index,
                               const char *language, uint32_t expected_codec,
                               ScryerFfmpegSyncDecodeResult *out)
{
    const enum AVCodecID expected = codec_id_for_sync_decode(expected_codec);
    if (requested_stream_index >= 0) {
        if ((unsigned)requested_stream_index >= format->nb_streams) {
            return -1;
        }
        AVStream *stream = format->streams[requested_stream_index];
        if (stream->codecpar->codec_type != AVMEDIA_TYPE_AUDIO) {
            return -1;
        }
        if (!codec_is_targeted(stream->codecpar->codec_id) ||
            (expected != AV_CODEC_ID_NONE && stream->codecpar->codec_id != expected)) {
            out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
            set_sync_decode_message(out, "requested audio stream codec is not handled by this sync decoder");
            return -2;
        }
        return requested_stream_index;
    }

    int first_targeted = -1;
    int default_targeted = -1;
    int language_targeted = -1;
    for (unsigned i = 0; i < format->nb_streams; i++) {
        AVStream *stream = format->streams[i];
        if (stream->codecpar->codec_type != AVMEDIA_TYPE_AUDIO ||
            !codec_is_targeted(stream->codecpar->codec_id)) {
            continue;
        }
        if (expected != AV_CODEC_ID_NONE && stream->codecpar->codec_id != expected) {
            continue;
        }
        if (first_targeted < 0) {
            first_targeted = (int)i;
        }
        if (stream->disposition & AV_DISPOSITION_DEFAULT) {
            default_targeted = (int)i;
        }
        if (stream_matches_language(stream, language)) {
            language_targeted = (int)i;
            break;
        }
    }

    if (language_targeted >= 0) {
        return language_targeted;
    }
    if (default_targeted >= 0) {
        return default_targeted;
    }
    if (first_targeted >= 0) {
        return first_targeted;
    }

    out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
    set_sync_decode_message(out, "no targeted audio stream found");
    return -2;
}

static int emit_pcm_samples(ScryerFfmpegPcmCallback callback, void *userdata,
                            const int16_t *samples, uintptr_t sample_count,
                            uint64_t max_output_samples,
                            ScryerFfmpegSyncDecodeResult *out)
{
    if (sample_count == 0) {
        return SCRYER_FFMPEG_DECODED;
    }
    if (!callback) {
        return set_sync_decode_error(out, "sync PCM consumer callback was null");
    }
    if (max_output_samples > 0 && out->samples_decoded >= max_output_samples) {
        return SCRYER_FFMPEG_DECODED;
    }

    uintptr_t emit_count = sample_count;
    if (max_output_samples > 0) {
        const uint64_t remaining = max_output_samples - out->samples_decoded;
        if (remaining < (uint64_t)emit_count) {
            emit_count = (uintptr_t)remaining;
        }
    }
    if (emit_count == 0) {
        return SCRYER_FFMPEG_DECODED;
    }

    if (callback(userdata, samples, emit_count, out->sample_rate_hz, out->channels) != 0) {
        return set_sync_decode_error(out, "sync PCM consumer callback failed");
    }
    out->samples_decoded += (uint64_t)emit_count;
    return SCRYER_FFMPEG_DECODED;
}

static int emit_silence(ScryerFfmpegPcmCallback callback, void *userdata,
                        int64_t samples, uint64_t max_output_samples,
                        ScryerFfmpegSyncDecodeResult *out)
{
    int16_t silence[4096] = {0};
    while (samples > 0) {
        const uintptr_t chunk = samples > 4096 ? 4096 : (uintptr_t)samples;
        int ret = emit_pcm_samples(callback, userdata, silence, chunk, max_output_samples, out);
        if (ret != SCRYER_FFMPEG_DECODED) {
            return ret;
        }
        if (max_output_samples > 0 && out->samples_decoded >= max_output_samples) {
            return SCRYER_FFMPEG_DECODED;
        }
        samples -= (int64_t)chunk;
    }
    return SCRYER_FFMPEG_DECODED;
}

static int convert_and_emit_frame(SwrContext *swr, const AVChannelLayout *output_layout,
                                  AVFrame *input, ScryerFfmpegPcmCallback callback,
                                  void *userdata, uint64_t max_output_samples,
                                  ScryerFfmpegSyncDecodeResult *out)
{
    const int64_t delay = swr_get_delay(swr, input->sample_rate);
    const int dst_samples = (int)av_rescale_rnd(delay + input->nb_samples,
                                                out->sample_rate_hz,
                                                input->sample_rate,
                                                AV_ROUND_UP);
    AVFrame *frame = av_frame_alloc();
    if (!frame) {
        return set_sync_decode_error(out, "failed to allocate resampled frame");
    }
    frame->nb_samples = dst_samples;
    frame->format = AV_SAMPLE_FMT_S16;
    frame->sample_rate = (int)out->sample_rate_hz;
    if (av_channel_layout_copy(&frame->ch_layout, output_layout) < 0) {
        av_frame_free(&frame);
        return set_sync_decode_error(out, "failed to set resampled channel layout");
    }
    int ret = av_frame_get_buffer(frame, 0);
    if (ret < 0) {
        av_frame_free(&frame);
        return set_sync_decode_av_error(out, "failed to allocate resampled buffer", ret);
    }
    ret = swr_convert(swr, frame->extended_data, dst_samples,
                      (const uint8_t **)input->extended_data, input->nb_samples);
    if (ret < 0) {
        av_frame_free(&frame);
        return set_sync_decode_av_error(out, "failed to resample decoded audio", ret);
    }
    frame->nb_samples = ret;
    ret = emit_pcm_samples(callback, userdata, (const int16_t *)frame->extended_data[0],
                           (uintptr_t)ret, max_output_samples, out);
    av_frame_free(&frame);
    return ret;
}

static int flush_resampler(SwrContext *swr, AVCodecContext *decoder,
                           const AVChannelLayout *output_layout,
                           ScryerFfmpegPcmCallback callback, void *userdata,
                           uint64_t max_output_samples,
                           ScryerFfmpegSyncDecodeResult *out)
{
    for (;;) {
        const int64_t delay = swr_get_delay(swr, decoder->sample_rate);
        if (delay <= 0) {
            return SCRYER_FFMPEG_DECODED;
        }
        const int dst_samples = (int)av_rescale_rnd(delay,
                                                    out->sample_rate_hz,
                                                    decoder->sample_rate,
                                                    AV_ROUND_UP);
        if (dst_samples <= 0) {
            return SCRYER_FFMPEG_DECODED;
        }
        AVFrame *frame = av_frame_alloc();
        if (!frame) {
            return set_sync_decode_error(out, "failed to allocate resampler flush frame");
        }
        frame->nb_samples = dst_samples;
        frame->format = AV_SAMPLE_FMT_S16;
        frame->sample_rate = (int)out->sample_rate_hz;
        if (av_channel_layout_copy(&frame->ch_layout, output_layout) < 0) {
            av_frame_free(&frame);
            return set_sync_decode_error(out, "failed to set resampler flush channel layout");
        }
        int ret = av_frame_get_buffer(frame, 0);
        if (ret < 0) {
            av_frame_free(&frame);
            return set_sync_decode_av_error(out, "failed to allocate resampler flush buffer", ret);
        }
        ret = swr_convert(swr, frame->extended_data, dst_samples, NULL, 0);
        if (ret < 0) {
            av_frame_free(&frame);
            return set_sync_decode_av_error(out, "failed to flush resampler", ret);
        }
        if (ret == 0) {
            av_frame_free(&frame);
            return SCRYER_FFMPEG_DECODED;
        }
        frame->nb_samples = ret;
        ret = emit_pcm_samples(callback, userdata, (const int16_t *)frame->extended_data[0],
                               (uintptr_t)ret, max_output_samples, out);
        av_frame_free(&frame);
        if (ret != SCRYER_FFMPEG_DECODED) {
            return ret;
        }
        if (max_output_samples > 0 && out->samples_decoded >= max_output_samples) {
            return SCRYER_FFMPEG_DECODED;
        }
    }
}

int32_t scryer_ffmpeg_decode_sync_audio(const char *input_path,
                                        int32_t requested_stream_index,
                                        const char *language,
                                        uint32_t expected_codec,
                                        uint64_t max_output_samples,
                                        ScryerFfmpegPcmCallback callback,
                                        void *userdata,
                                        ScryerFfmpegSyncDecodeResult *out)
{
    av_log_set_level(AV_LOG_QUIET);
    memset(out, 0, sizeof(*out));
    out->status_code = SCRYER_FFMPEG_ERROR;
    out->sample_rate_hz = 16000;
    out->channels = 1;
    out->timeline_start_ms = 0;

    AVFormatContext *ifmt = NULL;
    AVCodecContext *decoder = NULL;
    AVPacket *packet = NULL;
    AVFrame *frame = NULL;
    SwrContext *swr = NULL;
    AVChannelLayout output_layout = {0};

    int ret = avformat_open_input(&ifmt, input_path, NULL, NULL);
    if (ret < 0) {
        return set_sync_decode_av_error(out, "failed to open input media", ret);
    }
    ret = avformat_find_stream_info(ifmt, NULL);
    if (ret < 0) {
        set_sync_decode_av_error(out, "failed to read input stream info", ret);
        goto fail;
    }

    const int stream_index = select_audio_stream(ifmt, requested_stream_index, language,
                                                 expected_codec, out);
    if (stream_index == -2) {
        goto cleanup;
    }
    if (stream_index < 0) {
        set_sync_decode_error(out, "requested audio stream was not found");
        goto fail;
    }
    AVStream *istream = ifmt->streams[stream_index];
    AVCodecParameters *params = istream->codecpar;
    const int source_is_dts_hd_ma =
        params->codec_id == AV_CODEC_ID_DTS &&
        expected_codec == SCRYER_SYNC_CODEC_DTS_HD_MA_CORE;

    const enum AVCodecID expected = codec_id_for_sync_decode(expected_codec);
    if (expected != AV_CODEC_ID_NONE && params->codec_id != expected) {
        out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
        set_sync_decode_message(out, "selected audio stream did not match expected codec");
        goto cleanup;
    }
    if (!codec_is_targeted(params->codec_id)) {
        out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
        set_sync_decode_message(out, "selected audio codec is not handled by this sync decoder");
        goto cleanup;
    }

    const AVCodec *decoder_codec = avcodec_find_decoder(params->codec_id);
    if (!decoder_codec) {
        out->status_code = SCRYER_FFMPEG_UNSUPPORTED;
        set_sync_decode_message(out, "vendored FFmpeg decoder is not enabled");
        goto cleanup;
    }
    decoder = avcodec_alloc_context3(decoder_codec);
    if (!decoder) {
        set_sync_decode_error(out, "failed to allocate decoder context");
        goto fail;
    }
    ret = avcodec_parameters_to_context(decoder, params);
    if (ret < 0) {
        set_sync_decode_av_error(out, "failed to copy decoder parameters", ret);
        goto fail;
    }
    if (decoder->ch_layout.nb_channels <= 0) {
        av_channel_layout_default(&decoder->ch_layout, params->ch_layout.nb_channels > 0
                                                      ? params->ch_layout.nb_channels
                                                      : 2);
    }
    ret = avcodec_open2(decoder, decoder_codec, NULL);
    if (ret < 0) {
        set_sync_decode_av_error(out, "failed to open audio decoder", ret);
        goto fail;
    }
    if (decoder->sample_rate <= 0) {
        set_sync_decode_error(out, "decoded audio stream did not expose a sample rate");
        goto fail;
    }

    av_channel_layout_default(&output_layout, 1);
    ret = swr_alloc_set_opts2(&swr,
                              &output_layout,
                              AV_SAMPLE_FMT_S16,
                              (int)out->sample_rate_hz,
                              &decoder->ch_layout,
                              decoder->sample_fmt,
                              decoder->sample_rate,
                              0, NULL);
    if (ret < 0 || !swr) {
        set_sync_decode_av_error(out, "failed to allocate resampler", ret);
        goto fail;
    }
    ret = swr_init(swr);
    if (ret < 0) {
        set_sync_decode_av_error(out, "failed to initialize resampler", ret);
        goto fail;
    }

    packet = av_packet_alloc();
    frame = av_frame_alloc();
    if (!packet || !frame) {
        set_sync_decode_error(out, "failed to allocate decode packet/frame");
        goto fail;
    }

    int inserted_initial_timeline = 0;
    int reached_output_limit = 0;
    while (!reached_output_limit && (ret = av_read_frame(ifmt, packet)) >= 0) {
        if (packet->stream_index != stream_index) {
            av_packet_unref(packet);
            continue;
        }
        ret = avcodec_send_packet(decoder, packet);
        av_packet_unref(packet);
        if (ret < 0) {
            set_sync_decode_av_error(out, "failed to send audio packet to decoder", ret);
            goto fail;
        }
        for (;;) {
            ret = avcodec_receive_frame(decoder, frame);
            if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
                break;
            }
            if (ret < 0) {
                set_sync_decode_av_error(out, "failed to receive decoded audio frame", ret);
                goto fail;
            }
            if (!inserted_initial_timeline) {
                int64_t pts = frame->best_effort_timestamp;
                if (pts != AV_NOPTS_VALUE) {
                    int64_t pts_ms = av_rescale_q(pts, istream->time_base, (AVRational){1, 1000});
                    if (pts_ms > 0) {
                        int64_t silence_samples = av_rescale_q(pts_ms, (AVRational){1, 1000},
                                                               (AVRational){1, (int)out->sample_rate_hz});
                        ret = emit_silence(callback, userdata, silence_samples,
                                           max_output_samples, out);
                        if (ret != SCRYER_FFMPEG_DECODED) {
                            goto fail;
                        }
                    } else if (pts_ms < 0) {
                        out->timeline_start_ms = pts_ms;
                        snprintf(out->warnings, sizeof(out->warnings),
                                 "first decoded audio timestamp is negative; timeline_start_ms must be honored");
                    }
                }
                inserted_initial_timeline = 1;
            }
            ret = convert_and_emit_frame(swr, &output_layout, frame, callback, userdata,
                                         max_output_samples, out);
            av_frame_unref(frame);
            if (ret != SCRYER_FFMPEG_DECODED) {
                goto fail;
            }
            if (max_output_samples > 0 && out->samples_decoded >= max_output_samples) {
                reached_output_limit = 1;
                break;
            }
        }
    }
    if (!reached_output_limit && ret != AVERROR_EOF) {
        set_sync_decode_av_error(out, "failed while reading input media", ret);
        goto fail;
    }

    ret = reached_output_limit ? AVERROR_EOF : avcodec_send_packet(decoder, NULL);
    if (!reached_output_limit && ret >= 0) {
        for (;;) {
            ret = avcodec_receive_frame(decoder, frame);
            if (ret == AVERROR(EAGAIN) || ret == AVERROR_EOF) {
                break;
            }
            if (ret < 0) {
                set_sync_decode_av_error(out, "failed to flush audio decoder", ret);
                goto fail;
            }
            ret = convert_and_emit_frame(swr, &output_layout, frame, callback, userdata,
                                         max_output_samples, out);
            av_frame_unref(frame);
            if (ret != SCRYER_FFMPEG_DECODED) {
                goto fail;
            }
            if (max_output_samples > 0 && out->samples_decoded >= max_output_samples) {
                reached_output_limit = 1;
                break;
            }
        }
    }
    if (!reached_output_limit) {
        ret = flush_resampler(swr, decoder, &output_layout, callback, userdata,
                              max_output_samples, out);
        if (ret != SCRYER_FFMPEG_DECODED) {
            goto fail;
        }
    }
    if (out->samples_decoded == 0) {
        set_sync_decode_error(out, "FFmpeg decoder produced no sync PCM samples");
        goto fail;
    }

    out->status_code = SCRYER_FFMPEG_DECODED;
    out->stream_index = (uint32_t)stream_index;
    out->codec = sync_decode_codec_for_av(params->codec_id, source_is_dts_hd_ma);
    out->duration_ms = (int64_t)av_rescale_q((int64_t)out->samples_decoded,
                                             (AVRational){1, (int)out->sample_rate_hz},
                                             (AVRational){1, 1000});
    out->used_core_fallback = source_is_dts_hd_ma ? 1 : 0;
    snprintf(out->source_codec_name, sizeof(out->source_codec_name), "%s",
             avcodec_get_name(params->codec_id));
    const char *profile_name = params->profile == AV_PROFILE_UNKNOWN
                                   ? NULL
                                   : av_get_profile_name(decoder_codec, params->profile);
    if (profile_name) {
        snprintf(out->source_profile, sizeof(out->source_profile), "%s", profile_name);
    }
    const AVDictionaryEntry *lang = av_dict_get(istream->metadata, "language", NULL, 0);
    if (lang && lang->value) {
        snprintf(out->language, sizeof(out->language), "%s", lang->value);
    }
    if (out->used_core_fallback && !out->warnings[0]) {
        snprintf(out->warnings, sizeof(out->warnings),
                 "DTS-HD MA was decoded through the DTS core decoder path");
    }
    set_sync_decode_message(out, "decoded sync audio by vendored FFmpeg");
    goto cleanup;

fail:
    if (out->status_code != SCRYER_FFMPEG_ERROR) {
        out->status_code = SCRYER_FFMPEG_ERROR;
    }

cleanup:
    av_channel_layout_uninit(&output_layout);
    swr_free(&swr);
    av_frame_free(&frame);
    av_packet_free(&packet);
    avcodec_free_context(&decoder);
    avformat_close_input(&ifmt);
    return out->status_code;
}
