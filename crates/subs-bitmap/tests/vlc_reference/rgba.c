/* Compiles the original VLC YUVP converter, without copying or translating
 * its palette conversion or pixel loops. See adapter.h for callback types. */
#include "adapter.h"
#include VLC_YUVP_SOURCE
void reference_rgba(const video_format_t *format, picture_t *source, uint8_t *rgba) {
    filter_t filter = {0}; filter.fmt_in.video = *format;
    filter.fmt_out.video = *format; filter.fmt_out.video.i_chroma = VLC_CODEC_RGBA;
    assert(Open(&filter) == VLC_SUCCESS);
    picture_t destination = {0}; destination.Y_PIXELS = rgba; destination.Y_PITCH = format->i_width * 4;
    Convert(&filter, source, &destination);
}
