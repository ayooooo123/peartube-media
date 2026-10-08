//! oxideav-h264 against FFmpeg's h264 decoder, fed the packets FFmpeg's
//! demuxer and parser give its own decoder: every output frame's MD5
//! (planes packed without padding, in the decoder's pixel format) must
//! equal FFmpeg's framemd5, in order and in number.

use check_decoders::{decode_packets, ffmpeg_packets, tool};
use oxideav_core::{CodecId, CodecParameters, Frame};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Our frame MD5s for stream `v:0` of `path`.
fn ours(path: &Path) -> Vec<String> {
    // Length-prefixed (MP4-family) H.264 becomes Annex B with in-band
    // parameter sets, as FFmpeg's decoder reads it from extradata.
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let bsf = matches!(ext.as_str(), "mp4" | "mov" | "mkv" | "flv").then_some("h264_mp4toannexb");
    let packets = ffmpeg_packets(path, "v:0", bsf);
    let mut params = CodecParameters::video(CodecId::new("h264"));
    // FFmpeg's decoder starts from the reorder depth its probe measured
    // (codecpar->video_delay, which ffprobe prints as has_b_frames).
    let probe = tool(
        refcheck::pinned_ffprobe(),
        &["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=has_b_frames", "-of", "csv=p=0", path.to_str().unwrap()],
    );
    if let Some(delay) = String::from_utf8(probe).unwrap().lines().map(str::trim).find(|l| !l.is_empty()) {
        params.options.insert("video_delay", delay);
    }
    let (decoded, _refused) = decode_packets(&[oxideav_h264::register], &params, &packets);
    decoded
        .frames
        .iter()
        .filter_map(|frame| match frame {
            Frame::Video(video) => {
                let packed: Vec<u8> = video.image_planes().iter().flat_map(|plane| plane.data.iter().copied()).collect();
                Some(refcheck::md5_hex(&packed))
            }
            _ => None,
        })
        .collect()
}

/// FFmpeg's frame MD5s for stream `v:0` of `path`, in its decoder's own
/// pixel format (no conversion), or why FFmpeg could not produce them.
/// `-max_error_rate 1` keeps a damaged sample (attachment631) from
/// aborting FFmpeg, as FATE's command does with 0.96.
fn theirs(path: &Path) -> Result<Vec<String>, String> {
    let p = path.to_str().unwrap();
    let probe = tool(
        refcheck::pinned_ffprobe(),
        &["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt", "-of", "csv=p=0", p],
    );
    // A transport stream lists its streams once per program too.
    let probe = String::from_utf8(probe).unwrap();
    let pix_fmt = probe.lines().map(str::trim).find(|l| !l.is_empty()).ok_or("ffprobe: no pix_fmt")?;
    let args = refcheck::ffmpeg_video_md5_args(path, "0:v:0", pix_fmt, &["-max_error_rate", "1"]);
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin"])
        .args(&args)
        .output()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ffmpeg failed: {}", stderr.lines().last().unwrap_or("")));
    }
    Ok(refcheck::parse_framemd5(&String::from_utf8_lossy(&out.stdout)))
}

/// `h264_intra_first-small.ts` starts 12 fields before its first I
/// picture, which carries a recovery point SEI (recovery_frame_cnt 0);
/// its first SPS/PPS arrive with that picture. The B pictures decoded
/// after it but shown before it predict from pictures this decode never
/// had. FFmpeg marks a picture recovered at an IDR or a recovery point
/// (h264_slice.c:1677-1707) and withholds every picture output before
/// the first recovered one (h264_slice.c:1392-1402, h264dec.c:980-982).
/// The I picture's marking fails (its MMCOs name pictures before the
/// entry point), and the reorder depth FFmpeg's probe measured (2) keeps
/// the unmarked-random-access heuristic of its second field from
/// recovering the B pictures at once (h264_refs.c:815-826): 17 frames,
/// where the decoder used to output 20.
#[test]
fn pictures_before_the_recovery_point_are_withheld() {
    let path = refcheck::fate("h264/h264_intra_first-small.ts");
    let (ours, theirs) = (ours(&path), theirs(&path).unwrap());
    assert_eq!(ours.len(), theirs.len(), "frames: ours {} vs FFmpeg {}", ours.len(), theirs.len());
    assert_eq!(ours, theirs);
}

/// An x264 open-GOP stream without recovery point SEIs (I frames every 2 s,
/// three references, two B frames), entered at a non-IDR I frame as after
/// a seek. FFmpeg makes gap frames for the frame_nums it never saw (gray,
/// then copies), substitutes a default reference for missing list
/// entries, and judges the unmarked-random-access heuristic on the
/// references that leaves; it takes frames out for output by its
/// reorder depth, which decides the frames recovered after it.
///
/// The cuts are raw Annex B with the parameter sets in every packet, so
/// both decoders start from the same bytes (a container cut would carry
/// them in its avcC, which FFmpeg's Annex B filter adds only at IDR
/// pictures).
///
/// * At the 2 s I frame (frame_num 1): one gap frame, the heuristic
///   recovers it; FFmpeg shows 148 of the 152 frames. The decoder used
///   to drop the leading B frame (a missing reference) and then refuse
///   every later picture for its frame_num.
/// * At the 4 s I frame (frame_num 4): four gap frames fill the DPB, the
///   heuristic fails, and FFmpeg shows nothing.
#[test]
fn entering_an_open_gop_stream_at_a_non_idr_i_frame_matches_ffmpeg() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    let full = dir.join(format!("{}-opengop.mkv", std::process::id()));
    let x264 = "keyint=50:min-keyint=50:scenecut=0:open-gop=1:ref=3:bframes=2";
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=8"])
        .args(["-c:v", "libx264", "-preset", "medium", "-x264-params", x264])
        .args(["-bsf:v", "filter_units=remove_types=6", "-f", "matroska"])
        .arg(&full)
        .status()
        .unwrap();
    assert!(status.success());
    for (first_packet, frames) in [(48, 148), (100, 0)] {
        let cut = dir.join(format!("{}-opengop-from-{first_packet}.h264", std::process::id()));
        let filters = format!("noise=drop=lt(n\\,{first_packet}),h264_mp4toannexb,dump_extra=freq=all");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .arg(&full)
            .args(["-map", "0:v:0", "-c", "copy", "-bsf:v", &filters, "-f", "h264"])
            .arg(&cut)
            .status()
            .unwrap();
        assert!(status.success());
        let (ours, theirs) = (ours(&cut), theirs(&cut).unwrap());
        assert_eq!(theirs.len(), frames, "FFmpeg's frames from packet {first_packet}");
        assert_eq!(ours, theirs, "from packet {first_packet}");
    }
}

/// Every H.264 sample FFmpeg's FATE decodes (tests/fate/h264.mak: the
/// conformance suite and the feature samples, recovery-point streams
/// included). The reinit-* streams change pixel format mid-stream, which
/// a single framemd5 cannot hold, so they are not listed.
const FATE_SAMPLES: &[&str] = &[
    "h264-conformance/AUD_MW_E.264",
    "h264-conformance/BA1_FT_C.264",
    "h264-conformance/BA1_Sony_D.jsv",
    "h264-conformance/BA2_Sony_F.jsv",
    "h264-conformance/BA3_SVA_C.264",
    "h264-conformance/BAMQ1_JVC_C.264",
    "h264-conformance/BAMQ2_JVC_C.264",
    "h264-conformance/BANM_MW_D.264",
    "h264-conformance/BASQP1_Sony_C.jsv",
    "h264-conformance/BA_MW_D.264",
    "h264-conformance/CABA1_SVA_B.264",
    "h264-conformance/CABA1_Sony_D.jsv",
    "h264-conformance/CABA2_SVA_B.264",
    "h264-conformance/CABA2_Sony_E.jsv",
    "h264-conformance/CABA3_SVA_B.264",
    "h264-conformance/CABA3_Sony_C.jsv",
    "h264-conformance/CABA3_TOSHIBA_E.264",
    "h264-conformance/CABACI3_Sony_B.jsv",
    "h264-conformance/CABAST3_Sony_E.jsv",
    "h264-conformance/CABASTBR3_Sony_B.jsv",
    "h264-conformance/CABREF3_Sand_D.264",
    "h264-conformance/CACQP3_Sony_D.jsv",
    "h264-conformance/CAFI1_SVA_C.264",
    "h264-conformance/CAMA1_Sony_C.jsv",
    "h264-conformance/CAMA1_TOSHIBA_B.264",
    "h264-conformance/CAMA3_Sand_E.264",
    "h264-conformance/CAMACI3_Sony_C.jsv",
    "h264-conformance/CAMANL1_TOSHIBA_B.264",
    "h264-conformance/CAMANL2_TOSHIBA_B.264",
    "h264-conformance/CAMANL3_Sand_E.264",
    "h264-conformance/CAMASL3_Sony_B.jsv",
    "h264-conformance/CAMP_MOT_MBAFF_L30.26l",
    "h264-conformance/CAMP_MOT_MBAFF_L31.26l",
    "h264-conformance/CANL1_SVA_B.264",
    "h264-conformance/CANL1_Sony_E.jsv",
    "h264-conformance/CANL1_TOSHIBA_G.264",
    "h264-conformance/CANL2_SVA_B.264",
    "h264-conformance/CANL2_Sony_E.jsv",
    "h264-conformance/CANL3_SVA_B.264",
    "h264-conformance/CANL3_Sony_C.jsv",
    "h264-conformance/CANL4_SVA_B.264",
    "h264-conformance/CANLMA2_Sony_C.jsv",
    "h264-conformance/CANLMA3_Sony_C.jsv",
    "h264-conformance/CAPA1_TOSHIBA_B.264",
    "h264-conformance/CAPAMA3_Sand_F.264",
    "h264-conformance/CAPCM1_Sand_E.264",
    "h264-conformance/CAPCMNL1_Sand_E.264",
    "h264-conformance/CAPM3_Sony_D.jsv",
    "h264-conformance/CAQP1_Sony_B.jsv",
    "h264-conformance/CAWP1_TOSHIBA_E.264",
    "h264-conformance/CAWP5_TOSHIBA_E.264",
    "h264-conformance/CI1_FT_B.264",
    "h264-conformance/CI_MW_D.264",
    "h264-conformance/CVBS3_Sony_C.jsv",
    "h264-conformance/CVCANLMA2_Sony_C.jsv",
    "h264-conformance/CVFC1_Sony_C.jsv",
    "h264-conformance/CVFI1_SVA_C.264",
    "h264-conformance/CVFI1_Sony_D.jsv",
    "h264-conformance/CVFI2_SVA_C.264",
    "h264-conformance/CVFI2_Sony_H.jsv",
    "h264-conformance/CVMA1_Sony_D.jsv",
    "h264-conformance/CVMA1_TOSHIBA_B.264",
    "h264-conformance/CVMANL1_TOSHIBA_B.264",
    "h264-conformance/CVMANL2_TOSHIBA_B.264",
    "h264-conformance/CVMAPAQP3_Sony_E.jsv",
    "h264-conformance/CVMAQP2_Sony_G.jsv",
    "h264-conformance/CVMAQP3_Sony_D.jsv",
    "h264-conformance/CVMP_MOT_FLD_L30_B.26l",
    "h264-conformance/CVMP_MOT_FRM_L31_B.26l",
    "h264-conformance/CVNLFI1_Sony_C.jsv",
    "h264-conformance/CVNLFI2_Sony_H.jsv",
    "h264-conformance/CVPA1_TOSHIBA_B.264",
    "h264-conformance/CVPCMNL1_SVA_C.264",
    "h264-conformance/CVPCMNL2_SVA_C.264",
    "h264-conformance/CVWP1_TOSHIBA_E.264",
    "h264-conformance/CVWP2_TOSHIBA_E.264",
    "h264-conformance/CVWP3_TOSHIBA_E.264",
    "h264-conformance/CVWP5_TOSHIBA_E.264",
    "h264-conformance/FI1_Sony_E.jsv",
    "h264-conformance/FRext/FREXT01_JVC_D.264",
    "h264-conformance/FRext/FREXT02_JVC_C.264",
    "h264-conformance/FRext/FRExt1_Panasonic.avc",
    "h264-conformance/FRext/FRExt2_Panasonic.avc",
    "h264-conformance/FRext/FRExt3_Panasonic.avc",
    "h264-conformance/FRext/FRExt4_Panasonic.avc",
    "h264-conformance/FRext/FRExt_MMCO4_Sony_B.264",
    "h264-conformance/FRext/Freh12_B.264",
    "h264-conformance/FRext/Freh1_B.264",
    "h264-conformance/FRext/Freh2_B.264",
    "h264-conformance/FRext/Freh7_B.264",
    "h264-conformance/FRext/HCAFF1_HHI.264",
    "h264-conformance/FRext/HCAFR1_HHI.264",
    "h264-conformance/FRext/HCAFR2_HHI.264",
    "h264-conformance/FRext/HCAFR3_HHI.264",
    "h264-conformance/FRext/HCAFR4_HHI.264",
    "h264-conformance/FRext/HCAMFF1_HHI.264",
    "h264-conformance/FRext/HPCADQ_BRCM_B.264",
    "h264-conformance/FRext/HPCAFLNL_BRCM_C.264",
    "h264-conformance/FRext/HPCAFL_BRCM_C.264",
    "h264-conformance/FRext/HPCALQ_BRCM_B.264",
    "h264-conformance/FRext/HPCAMAPALQ_BRCM_B.264",
    "h264-conformance/FRext/HPCAMOLQ_BRCM_B.264",
    "h264-conformance/FRext/HPCANL_BRCM_C.264",
    "h264-conformance/FRext/HPCAQ2LQ_BRCM_B.264",
    "h264-conformance/FRext/HPCA_BRCM_C.264",
    "h264-conformance/FRext/HPCVFLNL_BRCM_A.264",
    "h264-conformance/FRext/HPCVFL_BRCM_A.264",
    "h264-conformance/FRext/HPCVMOLQ_BRCM_B.264",
    "h264-conformance/FRext/HPCVNL_BRCM_A.264",
    "h264-conformance/FRext/HPCV_BRCM_A.264",
    "h264-conformance/FRext/Hi422FR10_SONY_B.264",
    "h264-conformance/FRext/Hi422FR13_SONY_B.264",
    "h264-conformance/FRext/Hi422FR1_SONY_A.jsv",
    "h264-conformance/FRext/Hi422FR6_SONY_A.jsv",
    "h264-conformance/FRext/PPH10I1_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I2_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I3_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I4_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I5_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I6_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I7_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I1_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I2_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I3_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I4_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I5_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I6_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I7_Panasonic_A.264",
    "h264-conformance/FRext/freh10.264",
    "h264-conformance/FRext/freh11.264",
    "h264-conformance/FRext/freh3.264",
    "h264-conformance/FRext/freh4.264",
    "h264-conformance/FRext/freh5.264",
    "h264-conformance/FRext/freh6.264",
    "h264-conformance/FRext/freh8.264",
    "h264-conformance/FRext/freh9.264",
    "h264-conformance/FRext/test8b43.264",
    "h264-conformance/HCBP2_HHI_A.264",
    "h264-conformance/HCMP1_HHI_A.264",
    "h264-conformance/LS_SVA_D.264",
    "h264-conformance/MIDR_MW_D.264",
    "h264-conformance/MPS_MW_A.264",
    "h264-conformance/MR1_BT_A.h264",
    "h264-conformance/MR1_MW_A.264",
    "h264-conformance/MR2_MW_A.264",
    "h264-conformance/MR2_TANDBERG_E.264",
    "h264-conformance/MR3_TANDBERG_B.264",
    "h264-conformance/MR4_TANDBERG_C.264",
    "h264-conformance/MR5_TANDBERG_C.264",
    "h264-conformance/MR6_BT_B.h264",
    "h264-conformance/MR7_BT_B.h264",
    "h264-conformance/MR8_BT_B.h264",
    "h264-conformance/MR9_BT_B.h264",
    "h264-conformance/NL1_Sony_D.jsv",
    "h264-conformance/NL2_Sony_H.jsv",
    "h264-conformance/NL3_SVA_E.264",
    "h264-conformance/NLMQ1_JVC_C.264",
    "h264-conformance/NLMQ2_JVC_C.264",
    "h264-conformance/NRF_MW_E.264",
    "h264-conformance/SL1_SVA_B.264",
    "h264-conformance/SVA_BA1_B.264",
    "h264-conformance/SVA_BA2_D.264",
    "h264-conformance/SVA_Base_B.264",
    "h264-conformance/SVA_CL1_E.264",
    "h264-conformance/SVA_FM1_E.264",
    "h264-conformance/SVA_NL1_B.264",
    "h264-conformance/SVA_NL2_E.264",
    "h264-conformance/Sharp_MP_Field_1_B.jvt",
    "h264-conformance/Sharp_MP_Field_2_B.jvt",
    "h264-conformance/Sharp_MP_Field_3_B.jvt",
    "h264-conformance/Sharp_MP_PAFF_1r2.jvt",
    "h264-conformance/Sharp_MP_PAFF_2.jvt",
    "h264-conformance/cama1_vtc_c.avc",
    "h264-conformance/cama2_vtc_b.avc",
    "h264-conformance/cama3_vtc_b.avc",
    "h264-conformance/camp_mot_fld0_full.26l",
    "h264-conformance/camp_mot_frm0_full.26l",
    "h264-conformance/camp_mot_mbaff0_full.26l",
    "h264-conformance/camp_mot_picaff0_full.26l",
    "h264-conformance/cvmp_mot_fld0_full_B.26l",
    "h264-conformance/cvmp_mot_frm0_full_B.26l",
    "h264-conformance/cvmp_mot_mbaff0_full_B.26l",
    "h264-conformance/cvmp_mot_picaff0_full_B.26l",
    "h264-conformance/slice2_field_aurora4.264",
    "h264-conformance/src19td.IBP.264",
    "h264/SonyXAVC_LongGOP_green_pixelation_early_Frames.MXF",
    "h264/attachment631-small.mp4",
    "h264/bbc2.sample.h264",
    "h264/brokensps.flv",
    "h264/crew_cif_timecode-2.h264",
    "h264/crop-to-container-dims-canon.mov",
    "h264/data_partitioning.h264",
    "h264/data_partitioning_ab.h264",
    "h264/data_partitioning_cip.h264",
    "h264/direct-bff.mkv",
    "h264/dts_5frames.mkv",
    "h264/extradata-reload-multi-stsd.mov",
    "h264/extreme-plane-pred.h264",
    "h264/h264_intra_first-small.ts",
    "h264/h264refframeregression.mp4",
    "h264/interlaced_crop.mp4",
    "h264/intra_refresh.h264",
    "h264/lossless.h264",
    "h264/mixed-nal-coding.mp4",
    "h264/nondeterministic_cut.h264",
    "h264/ps_prefix_first_idr.mp4",
    "h264/ref-pic-mod-overflow.h264",
    "h264/thezerotheorem-cut.mp4",
    "h264/twofields_packet.mp4",
    "h264/unescaped_extradata.mp4",
];

/// Samples whose output differs from FFmpeg's for reasons other than
/// recovery handling, all already different before it (pixels from the
/// first differing frame on: MBAFF/PAFF inter, PCM, 4:2:2 intra, data
/// partitioning, lossless; counts: a flush error). They are still decoded
/// (a panic fails the test) and reported; one that starts to match is
/// reported too.
const KNOWN_DIVERGENT: &[&str] = &[
    "h264-conformance/CAMA1_TOSHIBA_B.264",
    "h264-conformance/CAMACI3_Sony_C.jsv",
    "h264-conformance/CAMANL1_TOSHIBA_B.264",
    "h264-conformance/CAPCM1_Sand_E.264",
    "h264-conformance/CAPCMNL1_Sand_E.264",
    "h264-conformance/CAPM3_Sony_D.jsv",
    "h264-conformance/CVMA1_TOSHIBA_B.264",
    "h264-conformance/CVMANL1_TOSHIBA_B.264",
    "h264-conformance/FRext/FREXT01_JVC_D.264",
    "h264-conformance/FRext/FREXT02_JVC_C.264",
    "h264-conformance/FRext/FRExt4_Panasonic.avc",
    "h264-conformance/FRext/HCAFF1_HHI.264",
    "h264-conformance/FRext/HCAFR2_HHI.264",
    "h264-conformance/FRext/HCAFR3_HHI.264",
    "h264-conformance/FRext/HCAMFF1_HHI.264",
    "h264-conformance/FRext/HPCAMAPALQ_BRCM_B.264",
    "h264-conformance/FRext/HPCAMOLQ_BRCM_B.264",
    "h264-conformance/FRext/HPCVMOLQ_BRCM_B.264",
    "h264-conformance/FRext/PPH10I6_Panasonic_A.264",
    "h264-conformance/FRext/PPH10I7_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I6_Panasonic_A.264",
    "h264-conformance/FRext/PPH422I7_Panasonic_A.264",
    "h264-conformance/FRext/freh5.264",
    "h264-conformance/Sharp_MP_PAFF_1r2.jvt",
    "h264-conformance/Sharp_MP_PAFF_2.jvt",
    "h264-conformance/cama1_vtc_c.avc",
    "h264-conformance/cama2_vtc_b.avc",
    "h264-conformance/cama3_vtc_b.avc",
    "h264-conformance/cvmp_mot_mbaff0_full_B.26l",
    "h264-conformance/cvmp_mot_picaff0_full_B.26l",
    "h264-conformance/slice2_field_aurora4.264",
    "h264/SonyXAVC_LongGOP_green_pixelation_early_Frames.MXF",
    "h264/attachment631-small.mp4",
    "h264/bbc2.sample.h264",
    "h264/brokensps.flv",
    "h264/crew_cif_timecode-2.h264",
    "h264/crop-to-container-dims-canon.mov",
    "h264/data_partitioning.h264",
    "h264/data_partitioning_ab.h264",
    "h264/data_partitioning_cip.h264",
    "h264/direct-bff.mkv",
    "h264/extradata-reload-multi-stsd.mov",
    "h264/h264refframeregression.mp4",
    "h264/interlaced_crop.mp4",
    "h264/lossless.h264",
];

/// Frame counts (ours, FFmpeg's) and how many leading frames are equal,
/// or why FFmpeg produced no reference.
fn compare(rel: &str) -> Result<(usize, usize, usize), String> {
    let path = refcheck::fate(rel);
    let theirs = theirs(&path)?;
    let ours = ours(&path);
    let same = ours.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
    Ok((ours.len(), theirs.len(), same))
}

#[test]
#[ignore = "decodes all 210 FATE H.264 samples (about half an hour on a loaded machine); \
            run with: cargo test -p check-decoders --test h264 -- --ignored --nocapture"]
fn fate_h264_samples_match_ffmpeg() {
    // Two workers (the machine is shared); each prints its rows as they
    // finish and returns the failures.
    let next = AtomicUsize::new(0);
    let failures: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    let mut failures = Vec::new();
                    while let Some(&rel) = FATE_SAMPLES.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let start = Instant::now();
                        let known = KNOWN_DIVERGENT.contains(&rel);
                        let (verdict, detail) = match compare(rel) {
                            Ok((ours, theirs, same)) if ours == theirs && same == ours => {
                                (if known { "MATCH (listed as divergent)" } else { "MATCH" }, format!("{ours} frames"))
                            }
                            Ok((ours, theirs, same)) => (
                                if known { "known" } else { "DIFF" },
                                format!("frames {ours} vs FFmpeg {theirs}, first {same} equal"),
                            ),
                            Err(e) => (if known { "known" } else { "NO-REFERENCE" }, e),
                        };
                        eprintln!("{verdict} {rel}: {detail} ({:.1} s)", start.elapsed().as_secs_f64());
                        if !known && !verdict.starts_with("MATCH") {
                            failures.push(format!("{rel}: {detail}"));
                        }
                    }
                    failures
                })
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("a decode panicked")).collect()
    });
    assert!(failures.is_empty(), "{} samples differ from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}
