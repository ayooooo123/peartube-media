/* The parts of VLC's vlc_common.h, vlc_tick.h and vlc_text_style.h that
 * modules/codec/cea708.c uses, with VLC's values, so the unmodified file
 * builds into a test oracle (tests/cea708.rs). From vlc-src 2e358f3,
 * LGPL-2.1-or-later. */
#ifndef VLC708_COMMON_H
#define VLC708_COMMON_H

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int64_t vlc_tick_t;
#define VLC_TICK_INVALID INT64_C(0)
#define CLOCK_FREQ INT64_C(1000000)
#define VLC_TICK_FROM_SEC(sec) (CLOCK_FREQ * (sec))
#define VLC_TICK_FROM_MS(ms) ((CLOCK_FREQ / INT64_C(1000)) * (ms))
#define MS_FROM_VLC_TICK(t) ((t) / INT64_C(1000))
static inline vlc_tick_t vlc_tick_from_samples(int64_t samples, unsigned samp_rate)
{
    return CLOCK_FREQ * samples / samp_rate;
}

#define ARRAY_SIZE(x) (sizeof(x) / sizeof((x)[0]))
#define likely(x) (x)
#define unlikely(x) (x)

#define STYLE_ALPHA_OPAQUE      0xFF
#define STYLE_ALPHA_TRANSPARENT 0x00
#define STYLE_NO_DEFAULTS               0x0
#define STYLE_HAS_FONT_COLOR            (1 << 0)
#define STYLE_HAS_FONT_ALPHA            (1 << 1)
#define STYLE_HAS_FLAGS                 (1 << 2)
#define STYLE_HAS_BACKGROUND_COLOR      (1 << 7)
#define STYLE_HAS_BACKGROUND_ALPHA      (1 << 8)
#define STYLE_ITALIC            (1 << 1)
#define STYLE_BACKGROUND        (1 << 4)
#define STYLE_UNDERLINE         (1 << 5)
#define STYLE_MONOSPACED        (1 << 8)
#define STYLE_BLINK_FOREGROUND  (1 << 10)
#define STYLE_BLINK_BACKGROUND  (1 << 11)

typedef struct
{
    uint16_t i_features;
    uint16_t i_style_flags;
    float f_font_relsize;
    uint32_t i_font_color;
    uint8_t i_font_alpha;
    uint32_t i_background_color;
    uint8_t i_background_alpha;
} text_style_t;

typedef struct text_segment_t text_segment_t;
struct text_segment_t
{
    char *psz_text;
    text_style_t *style;
    text_segment_t *p_next;
};

/* VLC's text_style_Create(STYLE_NO_DEFAULTS): a zero-filled style. */
static inline text_style_t *text_style_Create(int i_defaults)
{
    (void)i_defaults;
    return calloc(1, sizeof(text_style_t));
}

static inline text_segment_t *text_segment_New(const char *psz_text)
{
    text_segment_t *s = calloc(1, sizeof(*s));
    if (s && psz_text)
        s->psz_text = strdup(psz_text);
    return s;
}

static inline void text_segment_Delete(text_segment_t *s)
{
    if (!s)
        return;
    free(s->psz_text);
    free(s->style);
    free(s);
}

static inline void text_segment_ChainDelete(text_segment_t *s)
{
    while (s) {
        text_segment_t *next = s->p_next;
        text_segment_Delete(s);
        s = next;
    }
}

#endif
