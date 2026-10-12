/* The included source is the original LGPL-2.1-or-later VLC C decoder.
 * This file supplies input packets and serializes its decoded callbacks.
 * Input: VLCSUB01, u32le canvas width/height, then (i64le pts_us, u32le
 * payload length, payload) records. Output: VLCRGBA1, then (i64le start,
 * i64le stop, u32le x/y/region-width/region-height, full RGBA canvas).
 * There is no Rust decoder invocation or reimplemented parsing/RLE here. */
#include "adapter.h"
#include VLC_DECODER_SOURCE
static unsigned canvas_width, canvas_height;
static uint64_t read_le(unsigned size) {
    uint64_t value = 0;
    for (unsigned i = 0; i < size; i++) { int byte = getchar(); assert(byte != EOF); value |= (uint64_t)(unsigned char)byte << (i * 8); }
    return value;
}
static void write_le(uint64_t value, unsigned size) {
    for (unsigned i = 0; i < size; i++) assert(putchar((unsigned char)(value >> (i * 8))) != EOF);
}
void decoder_QueueSub(decoder_t *decoder, subpicture_t *sub) {
    (void)decoder;
    subpicture_region_t *region = sub->regions;
    assert(region && !region->next);
    size_t width = region->fmt.i_width, height = region->fmt.i_height;
    uint8_t *rgba = calloc(width * height, 4); assert(rgba);
    reference_rgba(&region->fmt, region->p_picture, rgba);
    uint8_t *canvas = calloc((size_t)canvas_width * canvas_height, 4); assert(canvas);
    // Placement only; the original converter produced every RGBA pixel.
    for (size_t y = 0; y < height; y++) {
        int64_t dst_y = region->i_y + (int64_t)y;
        if (dst_y < 0 || dst_y >= canvas_height) continue;
        for (size_t x = 0; x < width; x++) {
            int64_t dst_x = region->i_x + (int64_t)x;
            if (dst_x >= 0 && dst_x < canvas_width) memcpy(canvas + (dst_y * canvas_width + dst_x) * 4, rgba + (y * width + x) * 4, 4);
        }
    }
    write_le((uint64_t)sub->i_start, 8); write_le((uint64_t)sub->i_stop, 8);
    write_le(region->i_x, 4); write_le(region->i_y, 4); write_le(width, 4); write_le(height, 4);
    assert(fwrite(canvas, 4, (size_t)canvas_width * canvas_height, stdout) == (size_t)canvas_width * canvas_height);
    free(canvas); free(rgba); subpicture_Delete(sub);
}
int main(void) {
    char magic[8]; assert(fread(magic, 1, 8, stdin) == 8 && !memcmp(magic, "VLCSUB01", 8));
    canvas_width = read_le(4); canvas_height = read_le(4);
    assert(canvas_width && canvas_height && canvas_width <= 16384 && canvas_height <= 16384);
    assert((uint64_t)canvas_width * canvas_height * 4 <= (256 << 20));
    assert(fwrite("VLCRGBA1", 1, 8, stdout) == 8);
    es_format_t format = {0};
#ifdef REFERENCE_CVD
    format.i_codec = VLC_CODEC_CVD;
#else
    format.i_codec = VLC_CODEC_OGT;
#endif
    decoder_t decoder = {0}; decoder.fmt_in = &format;
    assert(DecoderOpen((vlc_object_t *)&decoder) == VLC_SUCCESS);
    for (;;) {
        int first = getchar(); if (first == EOF) break; assert(ungetc(first, stdin) == first);
        int64_t pts = (int64_t)read_le(8); size_t size = read_le(4); assert(size <= 65539);
        block_t *block = block_Alloc(size); block->i_pts = pts;
        assert(fread(block->p_buffer, 1, size, stdin) == size);
        assert(decoder.pf_decode(&decoder, block) == VLCDEC_SUCCESS);
    }
    DecoderClose((vlc_object_t *)&decoder);
    assert(!ferror(stdin) && fflush(stdout) == 0);
    return 0;
}
