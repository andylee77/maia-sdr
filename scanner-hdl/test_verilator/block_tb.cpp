// Fishball radio core - the 8-VSB block in Verilator against the model (doc/changes/085).
//
// Runs sync (62.5 MHz) and clk3x (three times it, edges aligned), feeds a capture's samples at
// 10 MSPS, loads the matched filter's table and enables the block. Then the model's schedule:
// each tap set (loads.bin, starting at field F) comes from a snapshot of field F - 1, so that
// snapshot is asked for once field F - 2 has been counted (at once for field 0), and the set is
// written and armed once field F - 1 has been counted. Reads both packet sources as the lane ring
// does and compares the codeword packets with the model's word for word, and the snapshot
// packets' values with the model's equalizer input. Writes a JSON summary.
//
// Usage: block_tb <vectors> <summary.json>
//
// SPDX-License-Identifier: MIT

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iterator>
#include <sstream>
#include <string>
#include <vector>

#include "Vblock.h"
#include "verilated.h"

#pragma pack(push, 1)
struct Field {
    uint64_t start;
    uint32_t found, inverted;
};
struct Load {
    uint32_t field, start;
    int16_t taps[128];
};
#pragma pack(pop)

template <typename T>
static std::vector<T> load(const std::string& path) {
    std::ifstream f(path, std::ios::binary);
    std::vector<char> b((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    std::vector<T> out(b.size() / sizeof(T));
    if (!out.empty()) memcpy(out.data(), b.data(), out.size() * sizeof(T));
    return out;
}

// One packet source's read side, as the lane ring drives it: a word a cycle, its data the cycle
// after, then the release.
struct Reader {
    bool reading = false;
    int next = 0;
    int issued = -1;
    bool releasing = false;
    std::vector<uint64_t> words;
    std::vector<std::vector<uint64_t>> packets;

    // Before a cycle: takes the data of the word issued the cycle before, and drives the next.
    void step(bool ready, uint64_t data, uint32_t& addr, uint8_t& en, uint8_t& release) {
        if (issued >= 0) words.push_back(data);
        release = 0;
        en = 0;
        issued = -1;
        if (releasing) {
            releasing = false;
            return;
        }
        if (!reading && ready) {
            reading = true;
            next = 0;
            words.clear();
        }
        if (!reading) return;
        if (next < 512) {
            addr = next;
            en = 1;
            issued = next++;
        } else if ((int)words.size() == 512) {
            packets.push_back(words);
            release = 1;
            reading = false;
            releasing = true;
        }
    }
};

int main(int argc, char** argv) {
    if (argc < 3) return 2;
    std::string dir = argv[1];
    std::string summary = argv[2];
    auto x = load<int16_t>(dir + "/x.bin");
    auto table = load<int16_t>(dir + "/mf_table.bin");
    auto y = load<int16_t>(dir + "/y.bin");
    auto fields = load<Field>(dir + "/fields.bin");
    auto loads = load<Load>(dir + "/loads.bin");
    auto model = load<uint64_t>(dir + "/packets.bin");
    std::ifstream mf(dir + "/meta.json");
    std::string meta((std::istreambuf_iterator<char>(mf)), std::istreambuf_iterator<char>());
    auto num = [&](const char* key) -> int64_t {
        auto at = meta.find(std::string("\"") + key + "\"");
        if (at == std::string::npos) return -1;
        at = meta.find(':', at);
        return strtoll(meta.c_str() + at + 1, nullptr, 10);
    };
    uint64_t step0 = (uint64_t)num("step0");
    size_t model_packets = model.size() / 512;

    VerilatedContext ctx;
    Vblock top{&ctx};
    // One sync cycle: six half-periods of clk3x, sync high for the first three.
    auto cycle = [&]() {
        for (int h = 0; h < 6; h++) {
            top.clk3x_clk = (h % 2 == 0);
            top.clk = (h < 3);
            top.eval();
        }
    };
    top.rst = 1;
    top.clk3x_rst = 1;
    for (int i = 0; i < 8; i++) cycle();
    top.rst = 0;
    top.clk3x_rst = 0;
    top.step0 = (uint32_t)step0;
    top.carrier0 = 0;
    for (int p = 0; p < 64; p++)
        for (int c = 0; c < 8; c++) {
            uint64_t w = 0;
            for (int d = 0; d < 5; d++) w |= (uint64_t)(table[p * 40 + 8 * d + c] & 0xFFF) << (12 * d);
            top.mf_wen = 1;
            top.mf_addr = p * 8 + c;
            top.mf_data = w;
            cycle();
        }
    top.mf_wen = 0;
    top.enable = 1;
    for (int i = 0; i < 8; i++) cycle();

    Reader cw, sn;
    size_t samples = x.size() / 2, k = 0;
    double due = 0;
    uint64_t n = 0;
    size_t next_load = 0, next_snap = 0;
    int tap_i = -1;
    while (k < samples) {
        // Samples at 10 MSPS: one every 6.25 sync cycles.
        if ((double)n >= due) {
            top.strobe_in = 1;
            top.re_in = (uint16_t)x[2 * k] & 0xFFF;
            top.im_in = (uint16_t)x[2 * k + 1] & 0xFFF;
            top.sample_count = k;
            k++;
            due += 6.25;
        } else {
            top.strobe_in = 0;
        }
        // Snapshots for the solves, then their tap sets.
        top.snapshot = 0;
        if (next_snap < loads.size() && !top.status_snapshot) {
            uint32_t snap = loads[next_snap].field - 1;
            if (snap == 0 || top.field_count >= snap) {
                top.snapshot = 1;
                next_snap++;
            }
        }
        top.tap_wen = 0;
        top.taps_arm = 0;
        if (tap_i < 0 && next_load < loads.size() && top.field_count >= loads[next_load].field && !top.status_armed)
            tap_i = 0;
        if (tap_i >= 0) {
            if (tap_i < 128) {
                top.tap_wen = 1;
                top.tap_addr = tap_i;
                top.tap_data = (uint16_t)loads[next_load].taps[tap_i];
                tap_i++;
            } else {
                top.taps_arm = 1;
                tap_i = -1;
                next_load++;
            }
        }
        uint32_t a;
        uint8_t e, r;
        a = top.cw_rd_addr;
        cw.step(top.cw_ready, top.cw_rd_data, a, e, r);
        top.cw_rd_addr = a;
        top.cw_rd_en = e;
        top.cw_release = r;
        a = top.sn_rd_addr;
        sn.step(top.sn_ready, top.sn_rd_data, a, e, r);
        top.sn_rd_addr = a;
        top.sn_rd_en = e;
        top.sn_release = r;
        cycle();
        n++;
    }

    // Codeword packets against the model's, word for word. The model's last one is cut short where
    // its capture ends: only its codewords are compared.
    uint64_t bad_packets = 0;
    std::ostringstream first;
    size_t compared = std::min(cw.packets.size(), model_packets);
    for (size_t p = 0; p < compared; p++) {
        const uint64_t* want = &model[p * 512];
        const auto& got = cw.packets[p];
        bool last = p + 1 == model_packets;
        int count = (int)((want[0] >> 32) & 0xFFFF);
        int bad_word = -1;
        for (int w = 0; w < 512; w++) {
            if (last && (w == 0 || w == 7 || w >= 8 + 26 * count)) continue;
            if (got[w] != want[w]) {
                bad_word = w;
                break;
            }
        }
        if (bad_word >= 0) {
            if (bad_packets < 8)
                first << (bad_packets ? ", " : "") << "\"packet " << p << " word " << bad_word << ": " << std::hex
                      << got[bad_word] << " / " << want[bad_word] << std::dec << "\"";
            bad_packets++;
        }
    }
    // Snapshot packets: each value against the model's equalizer input around its field sync.
    uint64_t bad_values = 0, snapshot_values = 0;
    std::ostringstream snap_first, snap_headers;
    for (const auto& pk : sn.packets) {
        uint64_t w0 = pk[0], w2 = pk[2];
        int count = (int)((w0 >> 32) & 0xFFFF);
        uint32_t field = (uint32_t)w2;
        int place = (int)((w2 >> 32) & 0xFFFF);
        snap_headers << (snap_headers.tellp() > 0 ? ", " : "") << "[" << field << ", " << place << ", " << count
                     << ", " << ((w0 >> 24) & 0xFF) << ", " << (w2 >> 48) << "]";
        if (field >= fields.size()) continue;
        uint64_t from = fields[field].start - 832 + place;
        for (int v = 0; v < count; v++) {
            int16_t got = (int16_t)(pk[8 + v / 4] >> (16 * (v % 4)));
            snapshot_values++;
            if (from + v >= y.size() || got != y[from + v]) {
                if (bad_values < 4)
                    snap_first << (bad_values ? ", " : "") << "\"field " << field << " value " << place + v << ": " << got
                               << " / " << (from + v < y.size() ? y[from + v] : 0) << "\"";
                bad_values++;
            }
        }
    }
    std::ofstream out(summary);
    out << "{\"sync_cycles\": " << n << ", \"samples\": " << k << ", \"codeword_packets\": " << cw.packets.size()
        << ", \"model_packets\": " << model_packets << ", \"compared\": " << compared << ", \"bad_packets\": " << bad_packets
        << ", \"first\": [" << first.str() << "], \"snapshot_packets\": " << sn.packets.size()
        << ", \"snapshot_values\": " << snapshot_values << ", \"bad_values\": " << bad_values << ", \"snapshot_first\": ["
        << snap_first.str() << "], \"snapshot_headers\": [" << snap_headers.str() << "], \"taps_loaded\": " << top.taps_loaded << ", \"fields\": " << top.field_count
        << ", \"lost\": " << top.lost << ", \"overrun\": " << (int)top.status_overrun << "}\n";
    return 0;
}
