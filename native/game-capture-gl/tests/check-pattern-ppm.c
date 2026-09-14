/* Validate the orientation and non-black colour regions of a P6 screenshot. */
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>

static int next_number(FILE *input, int *out) {
    int c;
    do {
        c = fgetc(input);
        if (c == '#') while ((c = fgetc(input)) != '\n' && c != EOF) {}
    } while (c != EOF && isspace((unsigned char)c));
    if (c == EOF || !isdigit((unsigned char)c)) return 0;
    int value = 0;
    do { value = value * 10 + (c - '0'); c = fgetc(input); } while (isdigit((unsigned char)c));
    if (c != EOF) ungetc(c, input);
    *out = value;
    return 1;
}

static int sample(const unsigned char *pixels, int width, int height, int x, int y,
                  unsigned long rgb[3]) {
    if (x < 6 || y < 6 || x + 6 >= width || y + 6 >= height) return 0;
    rgb[0] = rgb[1] = rgb[2] = 0;
    for (int dy = -6; dy <= 6; ++dy) for (int dx = -6; dx <= 6; ++dx) {
        const unsigned char *pixel = pixels + ((size_t)(y + dy) * (size_t)width + (size_t)(x + dx)) * 3;
        rgb[0] += pixel[0]; rgb[1] += pixel[1]; rgb[2] += pixel[2];
    }
    return 1;
}

static int dominates(const unsigned long rgb[3], int first, int second, int third) {
    return rgb[first] > rgb[second] + 35UL * 169UL && rgb[first] > rgb[third] + 35UL * 169UL;
}

static int above(const unsigned long rgb[3], int first, int second) {
    return rgb[first] > rgb[second] + 35UL * 169UL;
}

int main(int argc, char **argv) {
    if (argc != 2) return 64;
    FILE *input = fopen(argv[1], "rb");
    if (input == NULL) { perror(argv[1]); return 2; }
    int p = fgetc(input), six = fgetc(input), width, height, maxval;
    if (p != 'P' || six != '6' || !next_number(input, &width) || !next_number(input, &height) ||
        !next_number(input, &maxval) || width < 32 || height < 32 || maxval != 255) {
        fputs("invalid P6 image\n", stderr); fclose(input); return 2;
    }
    int separator = fgetc(input);
    if (!isspace((unsigned char)separator)) { fputs("missing P6 data separator\n", stderr); fclose(input); return 2; }
    size_t size = (size_t)width * (size_t)height * 3;
    unsigned char *pixels = malloc(size);
    if (pixels == NULL || fread(pixels, 1, size, input) != size) {
        fputs("could not read P6 pixels\n", stderr); free(pixels); fclose(input); return 2;
    }
    fclose(input);
    unsigned long tl[3], tr[3], bl[3], br[3];
    int valid = sample(pixels, width, height, width / 4, height / 4, tl) &&
                sample(pixels, width, height, width * 3 / 4, height / 4, tr) &&
                sample(pixels, width, height, width / 4, height * 3 / 4, bl) &&
                sample(pixels, width, height, width * 3 / 4, height * 3 / 4, br);
    free(pixels);
    /* PPM uses top-left origin: red, green, blue, yellow must appear in that order. */
    valid = valid && dominates(tl, 0, 1, 2) && dominates(tr, 1, 0, 2) &&
        dominates(bl, 2, 0, 1) && above(br, 0, 2) && above(br, 1, 2);
    if (!valid) {
        fprintf(stderr, "pattern mismatch: TL=%lu,%lu,%lu TR=%lu,%lu,%lu BL=%lu,%lu,%lu BR=%lu,%lu,%lu\n",
                tl[0],tl[1],tl[2], tr[0],tr[1],tr[2], bl[0],bl[1],bl[2], br[0],br[1],br[2]);
        return 1;
    }
    puts("pattern orientation and non-black colour samples verified");
    return 0;
}
