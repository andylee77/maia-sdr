// Fishball radio core - the 8-VSB front end in Verilator against the model (doc/changes/085).
//
// Feeds a capture's samples at the AD9361's pace (10 MSPS: 18.75 clk3x cycles a sample), loads
// the matched filter's table, and compares every symbol, early and late evaluation, segment and
// field record with the model's vectors, writing a JSON summary.
//
// Usage: front_tb <vectors> <summary.json> <symbols>
//
// SPDX-License-Identifier: MIT

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iterator>
#include <sstream>
#include <string>
#include <vector>

#include "Vfront.h"
#include "verilated.h"

#pragma pack(push, 1)
struct Trace {
    uint64_t pos;
    int32_t filtered[2];
    uint32_t gain;
    int32_t gained[2];
    uint32_t theta;
    int32_t turned[2];
    int32_t dc;
    int32_t y;
};
struct Side {
    uint32_t n, late;
    uint64_t pos;
    int32_t filtered[2];
    uint32_t theta;
    int32_t value;
};
struct Segment {
    uint64_t start, pos;
    int64_t freq, rate;
    uint32_t gain;
    int32_t dc;
    int64_t sync, timing, pn511;
};
struct Field {
    uint64_t start;
    uint32_t found, inverted;
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

static int64_t sx(uint64_t v, int bits) {
    uint64_t m = 1ull << (bits - 1);
    v &= (bits == 64) ? ~0ull : ((1ull << bits) - 1);
    return (int64_t)((v ^ m) - m);
}

struct Mismatches {
    uint64_t count = 0;
    std::vector<std::string> first;
    void add(const std::string& s) {
        if (count++ < 8) first.push_back(s);
    }
    std::string json(const char* name) const {
        std::ostringstream o;
        o << "\"" << name << "\": {\"mismatches\": " << count << ", \"first\": [";
        for (size_t i = 0; i < first.size(); i++) o << (i ? ", " : "") << "\"" << first[i] << "\"";
        o << "]}";
        return o.str();
    }
};

int main(int argc, char** argv) {
    if (argc < 4) return 2;
    std::string dir = argv[1];
    std::string summary = argv[2];
    uint64_t symbols = strtoull(argv[3], nullptr, 10);
    auto x = load<int16_t>(dir + "/x.bin");
    auto table = load<int16_t>(dir + "/mf_table.bin");
    auto trace = load<Trace>(dir + "/trace.bin");
    auto sides = load<Side>(dir + "/sides.bin");
    auto segments = load<Segment>(dir + "/segments.bin");
    auto fields = load<Field>(dir + "/fields.bin");
    std::ifstream mf(dir + "/meta.json");
    std::string meta((std::istreambuf_iterator<char>(mf)), std::istreambuf_iterator<char>());
    auto num = [&](const char* key, int64_t missing = -1) -> int64_t {
        auto at = meta.find(std::string("\"") + key + "\"");
        if (at == std::string::npos) return missing;
        at = meta.find(':', at);
        return strtoll(meta.c_str() + at + 1, nullptr, 10);
    };
    uint64_t step0 = (uint64_t)num("step0");
    // The carrier loop's starting frequency: the channel moved off the LO (vector sets without
    // one were made at the LO).
    int32_t carrier0 = (int32_t)num("carrier0", 0);
    int64_t settle = num("settle");
    int64_t first_segment = num("first_segment");

    VerilatedContext ctx;
    Vfront top{&ctx};
    auto tick = [&]() {
        top.clk = 0;
        top.eval();
        top.clk = 1;
        top.eval();
    };
    top.rst = 1;
    for (int i = 0; i < 4; i++) tick();
    top.rst = 0;
    top.step0 = (uint32_t)step0;
    top.carrier0 = (uint32_t)carrier0;
    for (int p = 0; p < 64; p++)
        for (int c = 0; c < 8; c++) {
            uint64_t w = 0;
            for (int d = 0; d < 5; d++) w |= (uint64_t)(table[p * 40 + 8 * d + c] & 0xFFF) << (12 * d);
            top.tw_en = 1;
            top.tw_addr = p * 8 + c;
            top.tw_data = w;
            tick();
        }
    top.tw_en = 0;

    Mismatches m_sym, m_tag, m_side, m_seg, m_field;
    uint64_t got_symbols = 0, got_sides = 0, got_segments = 0, got_fields = 0, lost = 0;
    size_t side_i = 0;
    size_t samples = x.size() / 2, k = 0;
    double due = 0;
    uint64_t cycle = 0;
    char buf[512];
    while (got_symbols < symbols && k < samples) {
        if (cycle >= due) {
            top.wr_en = 1;
            top.wr_re = (uint16_t)x[2 * k] & 0xFFF;
            top.wr_im = (uint16_t)x[2 * k + 1] & 0xFFF;
            k++;
            due += 18.75;
        } else {
            top.wr_en = 0;
        }
        tick();
        cycle++;
        if (top.y_valid) {
            uint64_t n = top.y_n;
            if (n != got_symbols) {
                snprintf(buf, sizeof buf, "symbol %llu came as %llu", (unsigned long long)got_symbols, (unsigned long long)n);
                m_sym.add(buf);
            }
            if (n < trace.size()) {
                const Trace& t = trace[n];
                uint64_t pos = ((uint64_t)top.y_pos[1] << 32) | top.y_pos[0];
                int64_t f0 = sx(top.t_filtered0, 13), f1 = sx(top.t_filtered1, 13);
                int64_t g0 = sx(top.t_gained0, 12), g1 = sx(top.t_gained1, 12);
                int64_t u0 = sx(top.t_turned0, 12), u1 = sx(top.t_turned1, 12);
                int64_t dc = sx(top.t_dc, 16), y = sx(top.y, 12);
                if (pos != t.pos || f0 != t.filtered[0] || f1 != t.filtered[1] || top.t_gain != t.gain ||
                    g0 != t.gained[0] || g1 != t.gained[1] || top.t_theta != t.theta || u0 != t.turned[0] ||
                    u1 != t.turned[1] || dc != t.dc || y != t.y) {
                    snprintf(buf, sizeof buf,
                             "symbol %llu: pos %llu/%llu filtered %lld,%lld/%d,%d gain %u/%u gained %lld,%lld/%d,%d "
                             "theta %u/%u turned %lld,%lld/%d,%d dc %lld/%d y %lld/%d",
                             (unsigned long long)n, (unsigned long long)pos, (unsigned long long)t.pos,
                             (long long)f0, (long long)f1, t.filtered[0], t.filtered[1], top.t_gain, t.gain,
                             (long long)g0, (long long)g1, t.gained[0], t.gained[1], top.t_theta, t.theta,
                             (long long)u0, (long long)u1, t.turned[0], t.turned[1], (long long)dc, t.dc,
                             (long long)y, t.y);
                    m_sym.add(buf);
                }
                // The symbol's place: the phase, its place in the segment, its field.
                int phase = (int64_t)n < settle ? 0 : (int64_t)n < settle + 64 * 832 ? 1 : 2;
                int place = phase == 2 ? (int)((((int64_t)n - first_segment) % 832 + 832) % 832) : -1;
                bool tag_ok = (int)top.y_phase == phase && (place < 0 || (int)top.y_place == place);
                for (size_t f = 0; f < fields.size(); f++) {
                    if (n >= fields[f].start && n < fields[f].start + 313 * 832) {
                        tag_ok = tag_ok && top.y_fields && top.y_field == f &&
                                 top.y_segment == (n - fields[f].start) / 832;
                        break;
                    }
                }
                if (!tag_ok) {
                    snprintf(buf, sizeof buf, "symbol %llu: phase %u place %u fields %u segment %u field %u",
                             (unsigned long long)n, top.y_phase, top.y_place, top.y_fields, top.y_segment, top.y_field);
                    m_tag.add(buf);
                }
            }
            got_symbols++;
        }
        if (top.side_valid) {
            if (side_i < sides.size()) {
                const Side& s = sides[side_i];
                int64_t f0 = sx(top.side_filtered0, 13), f1 = sx(top.side_filtered1, 13);
                int64_t v = sx(top.side_value, 12);
                if (top.side_n != s.n || top.side_late != s.late || f0 != s.filtered[0] || f1 != s.filtered[1] ||
                    top.side_theta != s.theta || v != s.value) {
                    snprintf(buf, sizeof buf, "side %zu (symbol %u/%u late %u/%u): filtered %lld,%lld/%d,%d theta %u/%u value %lld/%d",
                             side_i, top.side_n, s.n, top.side_late, s.late, (long long)f0, (long long)f1,
                             s.filtered[0], s.filtered[1], top.side_theta, s.theta, (long long)v, s.value);
                    m_side.add(buf);
                }
            }
            side_i++;
            got_sides++;
        }
        if (top.seg_valid) {
            if (got_segments < segments.size()) {
                const Segment& s = segments[got_segments];
                uint64_t pos = ((uint64_t)top.seg_pos[1] << 32) | top.seg_pos[0];
                int64_t freq = sx(((uint64_t)top.seg_freq), 35), rate = sx(top.seg_rate, 32);
                int64_t dc = sx(top.seg_dc, 16), sync = sx(top.seg_sync, 16), timing = sx(top.seg_timing, 17);
                int64_t pn = sx(top.seg_pn511, 22);
                if (top.seg_start != (uint32_t)s.start || pos != s.pos || freq != s.freq || rate != s.rate ||
                    top.seg_gain != s.gain || dc != s.dc || sync != s.sync || timing != s.timing || pn != s.pn511) {
                    snprintf(buf, sizeof buf,
                             "segment %llu: start %u/%llu pos %llu/%llu freq %lld/%lld rate %lld/%lld gain %u/%u dc %lld/%d "
                             "sync %lld/%lld timing %lld/%lld pn511 %lld/%lld",
                             (unsigned long long)got_segments, top.seg_start, (unsigned long long)s.start,
                             (unsigned long long)pos, (unsigned long long)s.pos, (long long)freq, (long long)s.freq,
                             (long long)rate, (long long)s.rate, top.seg_gain, s.gain, (long long)dc, s.dc,
                             (long long)sync, (long long)s.sync, (long long)timing, (long long)s.timing,
                             (long long)pn, (long long)s.pn511);
                    m_seg.add(buf);
                }
            }
            got_segments++;
        }
        if (top.field_valid) {
            if (got_fields < fields.size()) {
                const Field& f = fields[got_fields];
                if (top.field_no != got_fields || top.field_start != (uint32_t)f.start ||
                    top.field_found != f.found || top.field_inverted != f.inverted) {
                    snprintf(buf, sizeof buf, "field %llu: no %u start %u/%llu found %u/%u inverted %u/%u",
                             (unsigned long long)got_fields, top.field_no, top.field_start,
                             (unsigned long long)f.start, top.field_found, f.found, top.field_inverted, f.inverted);
                    m_field.add(buf);
                }
            }
            got_fields++;
        }
        if (top.field_lost) lost++;
    }
    std::ofstream out(summary);
    out << "{\"symbols\": " << got_symbols << ", \"sides\": " << got_sides << ", \"segments\": " << got_segments
        << ", \"fields\": " << got_fields << ", \"fields_lost\": " << lost << ", \"overrun\": " << (int)top.overrun
        << ", \"cycles\": " << cycle << ", " << m_sym.json("symbol") << ", " << m_tag.json("place") << ", "
        << m_side.json("side") << ", " << m_seg.json("segment") << ", " << m_field.json("field") << "}\n";
    return 0;
}
