/* The parts of VLC's vlc_subpicture.h that modules/codec/cea708.c uses.
 * From vlc-src 2e358f3, LGPL-2.1-or-later. */
#ifndef VLC708_SUBPICTURE_H
#define VLC708_SUBPICTURE_H

#include "vlc_common.h"

#define SUBPICTURE_ALIGN_LEFT       0x1
#define SUBPICTURE_ALIGN_RIGHT      0x2
#define SUBPICTURE_ALIGN_TOP        0x4
#define SUBPICTURE_ALIGN_BOTTOM     0x8

typedef struct subpicture_t
{
    vlc_tick_t i_start;
    vlc_tick_t i_stop;
    bool b_ephemer;
    bool b_subtitle;
    struct { void *sys; } updater;
} subpicture_t;

#endif
