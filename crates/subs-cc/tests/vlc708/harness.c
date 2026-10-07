/* Drives VLC's unmodified modules/codec/cea708.c the way
 * modules/codec/cc.c does for a CEA-708 caption track (service 1), and
 * prints every subpicture it queues.
 *
 * stdin: records of (int64 little-endian pts in microseconds, uint32
 * little-endian byte count, that many bytes of cc_data triplets), in
 * presentation order.
 * stdout: per subpicture
 *   OUT <start> <stop> <ephemer>
 *   REGION <origin.x bits> <origin.y bits> <flags> <align> <inner_align>
 *   SEG <text hex> <style_flags> <features> <font_color> <font_alpha>
 *       <background_color> <background_alpha> <relsize bits>
 *   END
 */
#include "vlc_codec.h"
#include "substext.h"
#include "cea708.h"

static decoder_t dec; /* zeroed: no picture size, as VLC's cc decoder */
static cea708_t *p_cea708;

subpicture_t *decoder_NewSubpictureText(decoder_t *p_dec)
{
    (void)p_dec;
    subpicture_t *p_spu = calloc(1, sizeof(*p_spu));
    subtext_updater_sys_t *sys = calloc(1, sizeof(*sys));
    if (!p_spu || !sys) {
        free(p_spu);
        free(sys);
        return NULL;
    }
    SubpictureUpdaterSysRegionInit(&sys->region);
    p_spu->updater.sys = sys;
    return p_spu;
}

static unsigned bits(float f)
{
    unsigned u;
    memcpy(&u, &f, sizeof u);
    return u;
}

void decoder_QueueSub(decoder_t *p_dec, subpicture_t *p_spu)
{
    (void)p_dec;
    subtext_updater_sys_t *sys = p_spu->updater.sys;
    printf("OUT %lld %lld %d\n", (long long)p_spu->i_start, (long long)p_spu->i_stop, p_spu->b_ephemer);
    for (substext_updater_region_t *r = &sys->region; r; r = r->p_next) {
        printf("REGION %08x %08x %d %d %d\n", bits(r->origin.x), bits(r->origin.y), r->flags, r->align,
               r->inner_align);
        for (text_segment_t *s = r->p_segments; s; s = s->p_next) {
            printf("SEG ");
            for (const unsigned char *c = (const unsigned char *)s->psz_text; c && *c; c++)
                printf("%02x", *c);
            const text_style_t *st = s->style;
            printf(" %u %u %06x %u %06x %u %08x\n", st->i_style_flags, st->i_features, st->i_font_color,
                   st->i_font_alpha, st->i_background_color, st->i_background_alpha, bits(st->f_font_relsize));
        }
    }
    printf("END\n");
    substext_updater_region_t *r = sys->region.p_next;
    text_segment_ChainDelete(sys->region.p_segments);
    while (r) {
        substext_updater_region_t *next = r->p_next;
        text_segment_ChainDelete(r->p_segments);
        free(r);
        r = next;
    }
    free(sys);
    free(p_spu);
}

/* cc.c DTVCC_ServiceData_Handler, channel 0: service 1. */
static void service_data(void *priv, uint8_t i_sid, vlc_tick_t i_time, const uint8_t *p_data, size_t i_data)
{
    (void)priv;
    if (i_sid == 1)
        CEA708_Decoder_Push(p_cea708, i_time, p_data, i_data);
}

static int read_exact(void *buf, size_t n)
{
    return fread(buf, 1, n, stdin) == n;
}

int main(void)
{
    cea708_demux_t *dtvcc = CEA708_DTVCC_Demuxer_New(&dec, service_data);
    p_cea708 = CEA708_Decoder_New(&dec);
    if (!dtvcc || !p_cea708)
        return 1;
    for (;;) {
        unsigned char head[12];
        if (!read_exact(head, sizeof head))
            break;
        int64_t pts = 0;
        for (int i = 7; i >= 0; i--)
            pts = (int64_t)((uint64_t)pts << 8 | head[i]);
        uint32_t n = (uint32_t)head[8] | (uint32_t)head[9] << 8 | (uint32_t)head[10] << 16 | (uint32_t)head[11] << 24;
        unsigned char *data = malloc(n ? n : 1);
        if (!data || !read_exact(data, n))
            return 1;
        /* cc.c Convert */
        unsigned i_ticks = 0;
        for (const unsigned char *p = data; p + 3 <= data + n; p += 3) {
            if (p[0] & 0x04) {
                const vlc_tick_t i_spupts = pts + vlc_tick_from_samples(i_ticks, 1200 / 3);
                if ((p[0] & 0x03) >= 2)
                    CEA708_DTVCC_Demuxer_Push(dtvcc, i_spupts, p);
            }
            i_ticks++;
        }
        free(data);
    }
    CEA708_Decoder_Release(p_cea708);
    CEA708_DTVCC_Demuxer_Release(dtvcc);
    return 0;
}
