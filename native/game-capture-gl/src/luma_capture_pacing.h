#pragma once
#include <stdint.h>

// Advance the original capture clock, not now + interval. Present timestamps
// are quantized by the game's rate; resetting the phase throws that fractional
// time away and can reduce 360-Hz capture to 250 Hz in a 500-Hz game.
// Skip missed deadlines without emitting duplicate/catch-up frames.
static inline int luma_capture_due(uint64_t now, uint64_t interval, uint64_t *due) {
    if (interval == 0 || now < *due) return 0;
    if (*due == 0) *due = now + interval;
    else *due += ((now - *due) / interval + 1) * interval;
    return 1;
}
