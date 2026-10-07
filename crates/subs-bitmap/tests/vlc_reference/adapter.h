/* Test-only callback/type adapter. Parsing, bit reading, RLE rendering and
 * YUVP conversion come from the unmodified VLC source included by the two
 * translation units, not from this adapter or from the Rust implementation. */
#ifndef PEARTUBE_VLC_REFERENCE_ADAPTER
#define PEARTUBE_VLC_REFERENCE_ADAPTER
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <limits.h>

typedef int64_t vlc_tick_t;
typedef uint32_t vlc_fourcc_t;
typedef struct { unsigned num, den; } vlc_rational_t;
#define VLC_API extern
#define VLC_USED
#define VLC_FOURCC(a,b,c,d) ((uint32_t)(a) | ((uint32_t)(b)<<8) | ((uint32_t)(c)<<16) | ((uint32_t)(d)<<24))
#include <vlc_fourcc.h>
#define VLC_SUCCESS 0
#define VLC_EGENERIC (-1)
#define VLC_ENOMEM (-2)
#define VLC_EINVAL (-3)
#define VLCDEC_SUCCESS 0
#define VLC_TICK_INVALID INT64_MIN
#define VLC_TICK_0 0
#define CLOCK_FREQ 1000000
#define MS_FROM_VLC_TICK(t) ((t)/1000)
#define __MIN(a,b) ((a)<(b)?(a):(b))
#define unlikely(v) (v)
#define N_(v) (v)
#define vlc_assert_unreachable() abort()
#define msg_Dbg(...) ((void)0)
#define msg_Warn(...) ((void)0)
#define msg_Err(...) ((void)0)
static inline uint32_t GetDWBE(const uint8_t *p) { return ((uint32_t)p[0]<<24)|((uint32_t)p[1]<<16)|((uint32_t)p[2]<<8)|p[3]; }
static inline uint16_t GetWBE(const uint8_t *p) { return ((uint16_t)p[0]<<8)|p[1]; }

/* Module registration does not participate in codec execution. The adapter
 * calls the actual DecoderOpen/Decode/DecoderClose and chroma Open/Convert. */
#define vlc_module_begin() static void reference_module_registration(void) {
#define vlc_module_end() }
#define set_description(...)
#define set_shortname(...)
#define set_subcategory(...)
#define set_capability(...)
#define set_callbacks(...)
#define add_submodule(...)
#define set_callback_video_converter(...)
#define set_callback_chroma_conv_probe(...)

#define VIDEO_PALETTE_COLORS_MAX 256
typedef struct { int i_entries; uint8_t palette[256][4]; } video_palette_t;
typedef struct {
    vlc_fourcc_t i_chroma;
    unsigned i_width, i_height, i_visible_width, i_visible_height;
    unsigned i_x_offset, i_y_offset, i_sar_num, i_sar_den, orientation;
    video_palette_t *p_palette;
} video_format_t;
typedef struct { uint8_t *p_pixels; int i_pitch; } plane_t;
typedef struct { plane_t p[4]; } picture_t;
#define Y_PIXELS p[0].p_pixels
#define U_PIXELS p[1].p_pixels
#define V_PIXELS p[2].p_pixels
#define A_PIXELS p[3].p_pixels
#define Y_PITCH p[0].i_pitch
#define U_PITCH p[1].i_pitch
#define V_PITCH p[2].i_pitch
#define A_PITCH p[3].i_pitch
typedef struct subpicture_region_t {
    struct subpicture_region_t *next;
    picture_t *p_picture;
    video_format_t fmt;
    int i_x, i_y;
    bool b_absolute, b_in_window;
} subpicture_region_t;
typedef struct { vlc_tick_t i_start, i_stop; bool b_ephemer; subpicture_region_t *regions; } subpicture_t;
typedef struct block_t {
    struct block_t *p_next;
    uint8_t *p_buffer, *allocation;
    size_t i_buffer;
    unsigned i_flags;
    vlc_tick_t i_pts, i_dts, i_length;
} block_t;
#define BLOCK_FLAG_CORRUPTED 1
typedef struct { vlc_fourcc_t i_codec; video_format_t video; } es_format_t;
typedef struct decoder_t {
    void *p_sys;
    es_format_t *fmt_in, fmt_out;
    int (*pf_decode)(struct decoder_t *, block_t *);
    block_t *(*pf_packetize)(struct decoder_t *, block_t **);
} decoder_t;
typedef decoder_t vlc_object_t;
typedef struct { es_format_t fmt_in, fmt_out; const void *ops; } filter_t;
#define VIDEO_FILTER_WRAPPER(name) static void name(filter_t *, picture_t *, picture_t *); static const int name##_ops = 0;
typedef struct { int unused; } vlc_chroma_conv_vec;
#define vlc_chroma_conv_add_in_outlist(...) ((void)0)

static inline void video_format_Init(video_format_t *f, vlc_fourcc_t c) { memset(f, 0, sizeof(*f)); f->i_chroma = c; }
static inline void video_format_Clean(video_format_t *f) { free(f->p_palette); f->p_palette = NULL; }
static inline block_t *block_Alloc(size_t size) {
    block_t *b = calloc(1, sizeof(*b)); assert(b);
    b->allocation = calloc(size + 64, 1); assert(b->allocation);
    b->p_buffer = b->allocation; b->i_buffer = size;
    b->i_pts = b->i_dts = VLC_TICK_INVALID;
    return b;
}
static inline void block_Release(block_t *b) { free(b->allocation); free(b); }
static inline void block_ChainRelease(block_t *b) { while (b) { block_t *next = b->p_next; block_Release(b); b = next; } }
static inline void block_ChainAppend(block_t **chain, block_t *b) { while (*chain) chain = &(*chain)->p_next; *chain = b; }
static inline block_t *block_ChainGather(block_t *chain) {
    size_t size = 0; for (block_t *b = chain; b; b = b->p_next) size += b->i_buffer;
    block_t *g = block_Alloc(size); g->i_pts = chain->i_pts; g->i_dts = chain->i_dts;
    size_t at = 0;
    for (block_t *b = chain; b; b = b->p_next) { memcpy(g->p_buffer + at, b->p_buffer, b->i_buffer); at += b->i_buffer; }
    block_ChainRelease(chain); return g;
}
static inline subpicture_t *decoder_NewSubpicture(decoder_t *d, void *unused) { (void)d; (void)unused; return calloc(1, sizeof(subpicture_t)); }
static inline subpicture_region_t *subpicture_region_New(const video_format_t *format) {
    subpicture_region_t *r = calloc(1, sizeof(*r)); assert(r);
    r->fmt = *format; r->fmt.p_palette = malloc(sizeof(video_palette_t)); assert(r->fmt.p_palette);
    *r->fmt.p_palette = *format->p_palette;
    r->p_picture = calloc(1, sizeof(picture_t)); assert(r->p_picture);
    assert(format->i_width && format->i_height && format->i_width <= 16384 && format->i_height <= 16384);
    r->p_picture->Y_PIXELS = calloc(format->i_width, format->i_height); assert(r->p_picture->Y_PIXELS);
    r->p_picture->Y_PITCH = format->i_width; return r;
}
static inline void vlc_spu_regions_push(subpicture_region_t **regions, subpicture_region_t *r) { while (*regions) regions = &(*regions)->next; *regions = r; }
static inline void subpicture_Delete(subpicture_t *s) {
    subpicture_region_t *r = s->regions;
    while (r) { subpicture_region_t *next = r->next; free(r->p_picture->Y_PIXELS); free(r->p_picture); free(r->fmt.p_palette); free(r); r = next; }
    free(s);
}
void decoder_QueueSub(decoder_t *, subpicture_t *);
void picture_CopyProperties(picture_t *, const picture_t *);
void picture_Release(picture_t *);
void reference_rgba(const video_format_t *, picture_t *, uint8_t *);
#endif
