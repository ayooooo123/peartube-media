// Test-only adapter for libopenmpt 0.8.9's internal playback API.
// Test-only: matching upstream headers and static library are required.
// No Rust crate compiles or links this adapter. The public API has no seed ctl.
// This version-pinned white-box reference exposes
// the protected native PRNG through a standard C++ pointer-to-member, without
// changing the library, its mixing code, or any input random controls.
#include "common/stdafx.h"
#include "soundlib/Sndfile.h"
#include "libopenmpt/libopenmpt_impl.hpp"
#include <array>
#include <bit>
#include <cstdint>
#include <fstream>
#include <iostream>
#include <iterator>
#include <limits>
#include <stdexcept>
#include <string>
#include <vector>

struct PlaybackSeedAccess : OpenMPT::CSoundFile {
    static auto state_member() { return &PlaybackSeedAccess::m_PRNG; }
};

class SeededModule : public openmpt::module_impl {
public:
    using module_impl::module_impl;
    void set_seed(std::uint32_t seed) {
        m_sndFile.get()->*PlaybackSeedAccess::state_member() = OpenMPT::mpt::fast_prng(seed);
    }
};

int main(int argc, char **argv) {
    try {
        if(argc != 4) throw std::runtime_error("usage: tracker-seeded-oracle MODULE OUTPUT.f32 SEED");
        static_assert(std::endian::native == std::endian::little);
        const auto version = openmpt::string::get("library_version");
        if(version.find("0.8.9") != 0) throw std::runtime_error("requires libopenmpt 0.8.9: " + version);
        const auto value = std::stoull(argv[3]);
        if(value > std::numeric_limits<std::uint32_t>::max()) throw std::runtime_error("seed exceeds u32");
        const auto seed = static_cast<std::uint32_t>(value);
        std::ifstream input(argv[1], std::ios::binary);
        if(!input) throw std::runtime_error("cannot open input");
        std::vector<char> data{std::istreambuf_iterator<char>(input), std::istreambuf_iterator<char>()};
        SeededModule mod(data, std::make_unique<openmpt::std_ostream_log>(std::cerr), {});
        mod.select_subsong(-1);
        mod.set_render_param(openmpt::module::RENDER_MASTERGAIN_MILLIBEL, 0);
        mod.set_render_param(openmpt::module::RENDER_STEREOSEPARATION_PERCENT, 100);
        mod.set_render_param(openmpt::module::RENDER_INTERPOLATIONFILTER_LENGTH, 8);
        mod.set_render_param(openmpt::module::RENDER_VOLUMERAMPING_STRENGTH, -1);
        mod.set_seed(seed);
        std::ofstream output(argv[2], std::ios::binary);
        if(!output) throw std::runtime_error("cannot open output");
        std::array<float, 2048> pcm;
        std::uint64_t frames = 0;
        while(const auto count = mod.read_interleaved_stereo(48000, 1024, pcm.data())) {
            output.write(reinterpret_cast<const char *>(pcm.data()), count * 2 * sizeof(float));
            if(!output) throw std::runtime_error("cannot write PCM");
            frames += count;
        }
        std::cerr << "libopenmpt=" << version << " seed=" << seed << " frames=" << frames << '\n';
    } catch(const std::exception &error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
