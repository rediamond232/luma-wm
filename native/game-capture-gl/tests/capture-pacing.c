#include "../src/luma_capture_pacing.h"
#include <assert.h>
#include <stdio.h>

static void check_rate(unsigned source_hz, unsigned target_hz) {
    uint64_t due = 0;
    unsigned captured = 0;
    for (unsigned frame = 0; frame < source_hz * 10; ++frame) {
        const uint64_t pts = 1000000000ull + (uint64_t)frame * 1000000000ull / source_hz;
        captured += luma_capture_due(pts, 1000000000ull / target_hz, &due);
    }
    const unsigned wanted = (source_hz < target_hz ? source_hz : target_hz) * 10;
    assert(captured >= wanted - 1 && captured <= wanted + 1);
    printf("source=%u target=%u captured=%u in 10 seconds\n", source_hz, target_hz, captured);
}
int main(void) {
    check_rate(500, 360);
    check_rate(774, 360);
    check_rate(1000, 480);
    check_rate(500, 240);
    check_rate(220, 360);
    uint64_t due = 0;
    assert(luma_capture_due(1000, 10, &due));
    assert(luma_capture_due(2000, 10, &due));
    assert(!luma_capture_due(2000, 10, &due));
    assert(!luma_capture_due(2001, 10, &due));
    assert(luma_capture_due(2010, 10, &due));
    puts("PASS: phase-preserving pacing without catch-up duplicates");
}
