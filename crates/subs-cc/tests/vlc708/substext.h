/* The parts of VLC's modules/codec/substext.h that cea708.c uses: the
 * updater regions, initialized as SubpictureUpdaterSysRegionInit does.
 * From vlc-src 2e358f3, LGPL-2.1-or-later. */
#ifndef VLC708_SUBSTEXT_H
#define VLC708_SUBSTEXT_H

#include "vlc_subpicture.h"

typedef struct substext_updater_region_t substext_updater_region_t;

enum substext_updater_region_flags_e
{
    UPDT_REGION_ORIGIN_X_IS_RATIO      = 1 << 0,
    UPDT_REGION_ORIGIN_Y_IS_RATIO      = 1 << 1,
    UPDT_REGION_EXTENT_X_IS_RATIO      = 1 << 2,
    UPDT_REGION_EXTENT_Y_IS_RATIO      = 1 << 3,
    UPDT_REGION_IGNORE_BACKGROUND      = 1 << 4,
    UPDT_REGION_USES_GRID_COORDINATES  = 1 << 5,
    UPDT_REGION_USES_16_9_GRID         = 1 << 6,
};

struct substext_updater_region_t
{
    struct
    {
        float x;
        float y;
    } origin, extent;
    int flags;
    int align;
    bool b_absolute;
    bool b_in_window;
    int inner_align;
    text_segment_t *p_segments;
    substext_updater_region_t *p_next;
};

typedef struct
{
    substext_updater_region_t region;
    text_style_t *p_default_style;
    float margin_ratio;
} subtext_updater_sys_t;

static inline void SubpictureUpdaterSysRegionInit(substext_updater_region_t *p_updtregion)
{
    memset(p_updtregion, 0, sizeof(*p_updtregion));
    p_updtregion->align = SUBPICTURE_ALIGN_BOTTOM;
    p_updtregion->b_absolute = false;
    p_updtregion->b_in_window = false;
    p_updtregion->inner_align = 0;
}

static inline substext_updater_region_t *SubpictureUpdaterSysRegionNew(void)
{
    substext_updater_region_t *p_region = malloc(sizeof(*p_region));
    if (p_region)
        SubpictureUpdaterSysRegionInit(p_region);
    return p_region;
}

static inline void SubpictureUpdaterSysRegionAdd(substext_updater_region_t *p_prev,
                                                 substext_updater_region_t *p_new)
{
    substext_updater_region_t **pp_next = &p_prev->p_next;
    for (; *pp_next; pp_next = &(*pp_next)->p_next)
        ;
    *pp_next = p_new;
}

#endif
