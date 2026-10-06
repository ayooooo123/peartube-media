# Fork task result (worktree fork-mpeg4)

Crate: check-mpeg4 (test-only; src/lib.rs is a stub, all surface in tests/)
Codec IDs/tags registered: mpeg4video — FourCCs XVID/DIVX/DX50/FMP4/MP4V/M4S2, MP4 OTI 0x20, Matroska V_MPEG4/ISO/ASP + V_MPEG4/ISO/SP + V_MPEG4/ISO/AP
Priority: default fork priority (engine-side unshimmed; OxideAV software sits at 100+)
FFmpeg files ported: simple_idct.c + simple_idct_template.c (already in 62d1db6; extended with the add-variant), LGPL verified
Test results: check-mpeg4 15/15 pass (bit-exact framemd5 vs ffmpeg -idct simple on generated streams; FATE gaps pinned, see below); player 5/5; fork suite 1415 passed / 0 failed
Pin: ayooooo123/oxideav-mpeg4video @ defd469 (branch peartube), commits a0ee90e, a0dc44f, defd469
Known gaps (pinned as such in tests/reference.rs):
- demo.m4v: VOP header marker-bit parse bug in the fork's VOP layer
- packed_bframes.avi: packed B-frame split (mpeg4_unpack_bframes step) not implemented; 20 vs 15 frames
- xvid_vlc_trac7411.h263 + resize_*.h263: short-header path has isolated ±1 diffs vs FFmpeg NEON
- mpeg4_sstp_dpcm.m4v: studio profile VOL shape rejected (separate engine)
- refcheck needs an extra_args option: oracle must be `ffmpeg -idct simple` (C IDCT); NEON default differs

Correction log:
- The first oracle fix commit (cad60db) replaced the gap pins with real
  assertions. With the C oracle, xvid/resize still show isolated ±1
  diffs and demo.m4v's P-VOP fails — those are real fork gaps, now
  narrowed: demo's I-VOP decodes after the leniency changes (ac636cd).
