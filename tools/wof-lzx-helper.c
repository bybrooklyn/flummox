/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* Length-prefixed, round-trip-verified LZX helper for pack benchmarks. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <wimlib.h>

#define CHUNK_BYTES (32u * 1024u)

static int read_all(void *buffer, size_t size)
{
    return fread(buffer, 1, size, stdin) == size;
}

static int write_all(const void *buffer, size_t size)
{
    return fwrite(buffer, 1, size, stdout) == size && fflush(stdout) == 0;
}

static int read_u32(uint32_t *value)
{
    uint8_t bytes[4];
    if (!read_all(bytes, sizeof(bytes))) {
        return 0;
    }
    *value = (uint32_t)bytes[0] |
             ((uint32_t)bytes[1] << 8) |
             ((uint32_t)bytes[2] << 16) |
             ((uint32_t)bytes[3] << 24);
    return 1;
}

static int write_u32(uint32_t value)
{
    const uint8_t bytes[4] = {
        (uint8_t)value,
        (uint8_t)(value >> 8),
        (uint8_t)(value >> 16),
        (uint8_t)(value >> 24),
    };
    return write_all(bytes, sizeof(bytes));
}

int main(void)
{
    struct wimlib_compressor *compressor = NULL;
    struct wimlib_decompressor *decompressor = NULL;
    uint8_t *input = NULL;
    uint8_t *compressed = NULL;
    uint8_t *decoded = NULL;
    int result = 1;

    if (wimlib_create_compressor(WIMLIB_COMPRESSION_TYPE_LZX,
                                 CHUNK_BYTES, 50, &compressor) != 0 ||
        wimlib_create_decompressor(WIMLIB_COMPRESSION_TYPE_LZX,
                                   CHUNK_BYTES, &decompressor) != 0) {
        fputs("could not initialize wimlib LZX\n", stderr);
        goto out;
    }
    input = malloc(CHUNK_BYTES);
    compressed = malloc(CHUNK_BYTES);
    decoded = malloc(CHUNK_BYTES);
    if (input == NULL || compressed == NULL || decoded == NULL) {
        fputs("could not allocate codec buffers\n", stderr);
        goto out;
    }

    for (;;) {
        uint32_t length = 0;
        if (!read_u32(&length)) {
            fputs("truncated chunk header\n", stderr);
            goto out;
        }
        if (length == 0) {
            result = 0;
            break;
        }
        if (length > CHUNK_BYTES || !read_all(input, length)) {
            fputs("invalid or truncated chunk\n", stderr);
            goto out;
        }
        size_t stored = wimlib_compress(input, length, compressed,
                                        length - 1, compressor);
        if (stored != 0) {
            if (wimlib_decompress(compressed, stored, decoded,
                                  length, decompressor) != 0 ||
                memcmp(input, decoded, length) != 0) {
                fputs("LZX round trip failed\n", stderr);
                goto out;
            }
        } else {
            stored = length;
        }
        if (!write_u32((uint32_t)stored)) {
            fputs("could not return chunk size\n", stderr);
            goto out;
        }
    }

out:
    free(decoded);
    free(compressed);
    free(input);
    wimlib_free_decompressor(decompressor);
    wimlib_free_compressor(compressor);
    return result;
}
