/* The parts of VLC's vlc_codec.h that modules/codec/cea708.c uses. The
 * caption decoder's output format has no picture size, as VLC's cc
 * decoder leaves it. From vlc-src 2e358f3, LGPL-2.1-or-later. */
#ifndef VLC708_CODEC_H
#define VLC708_CODEC_H

#include "vlc_subpicture.h"

typedef struct
{
    unsigned i_visible_width;
    unsigned i_visible_height;
    unsigned i_sar_num;
    unsigned i_sar_den;
} video_format_t;

typedef struct
{
    video_format_t video;
} es_format_t;

typedef struct decoder_t
{
    es_format_t fmt_out;
} decoder_t;

subpicture_t *decoder_NewSubpictureText(decoder_t *p_dec);
void decoder_QueueSub(decoder_t *p_dec, subpicture_t *p_spu);

#endif
