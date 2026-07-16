#include <cuda_runtime.h>
#include <stdint.h>
#include <stddef.h>

#ifndef EMBER_CUDA_THREADS
#define EMBER_CUDA_THREADS 256
#endif

#define KECCAK_RATE 136
#define MAX_PREFIX_LEN 320

__constant__ uint8_t C_PREFIX[MAX_PREFIX_LEN];
__constant__ uint8_t C_TARGET[32];

static __device__ __forceinline__ uint64_t rotl64(uint64_t x, int n) {
    if (n == 0) return x;
    return (x << n) | (x >> (64 - n));
}

static __device__ __forceinline__ uint64_t keccak_rc(int round) {
    const uint64_t rc[24] = {
        0x0000000000000001ULL, 0x0000000000008082ULL, 0x800000000000808aULL,
        0x8000000080008000ULL, 0x000000000000808bULL, 0x0000000080000001ULL,
        0x8000000080008081ULL, 0x8000000000008009ULL, 0x000000000000008aULL,
        0x0000000000000088ULL, 0x0000000080008009ULL, 0x000000008000000aULL,
        0x000000008000808bULL, 0x800000000000008bULL, 0x8000000000008089ULL,
        0x8000000000008003ULL, 0x8000000000008002ULL, 0x8000000000000080ULL,
        0x000000000000800aULL, 0x800000008000000aULL, 0x8000000080008081ULL,
        0x8000000000008080ULL, 0x0000000080000001ULL, 0x8000000080008008ULL
    };
    return rc[round];
}

static __device__ void keccak_f1600(uint64_t s[25]) {
    const int rot[25] = {
        0, 1, 62, 28, 27,
        36, 44, 6, 55, 20,
        3, 10, 43, 25, 39,
        41, 45, 15, 21, 8,
        18, 2, 61, 56, 14
    };

    for (int round = 0; round < 24; ++round) {
        uint64_t c[5], d[5], b[25];

        for (int x = 0; x < 5; ++x) {
            c[x] = s[x] ^ s[x + 5] ^ s[x + 10] ^ s[x + 15] ^ s[x + 20];
        }
        for (int x = 0; x < 5; ++x) {
            d[x] = c[(x + 4) % 5] ^ rotl64(c[(x + 1) % 5], 1);
        }
        for (int x = 0; x < 5; ++x) {
            for (int y = 0; y < 5; ++y) {
                s[x + 5 * y] ^= d[x];
            }
        }

        for (int x = 0; x < 5; ++x) {
            for (int y = 0; y < 5; ++y) {
                int src = x + 5 * y;
                int nx = y;
                int ny = (2 * x + 3 * y) % 5;
                b[nx + 5 * ny] = rotl64(s[src], rot[src]);
            }
        }

        for (int x = 0; x < 5; ++x) {
            for (int y = 0; y < 5; ++y) {
                s[x + 5 * y] =
                    b[x + 5 * y] ^ ((~b[((x + 1) % 5) + 5 * y]) & b[((x + 2) % 5) + 5 * y]);
            }
        }

        s[0] ^= keccak_rc(round);
    }
}

static __device__ __forceinline__ void absorb_byte(uint64_t state[25], uint32_t &pos, uint8_t byte) {
    state[pos >> 3] ^= ((uint64_t)byte) << ((pos & 7) * 8);
    pos++;
    if (pos == KECCAK_RATE) {
        keccak_f1600(state);
        pos = 0;
    }
}

static __device__ int u64_to_decimal(uint64_t value, char out[20]) {
    char tmp[20];
    int len = 0;
    do {
        tmp[len++] = (char)('0' + (value % 10ULL));
        value /= 10ULL;
    } while (value != 0ULL);

    for (int i = 0; i < len; ++i) {
        out[i] = tmp[len - 1 - i];
    }
    return len;
}

static __device__ void keccak_json_nonce(
    uint32_t prefix_len,
    uint64_t nonce,
    uint8_t out[32]
) {
    uint64_t state[25];
    for (int i = 0; i < 25; ++i) state[i] = 0ULL;

    uint32_t pos = 0;
    for (uint32_t i = 0; i < prefix_len; ++i) {
        absorb_byte(state, pos, C_PREFIX[i]);
    }

    char digits[20];
    int digit_len = u64_to_decimal(nonce, digits);
    for (int i = 0; i < digit_len; ++i) {
        absorb_byte(state, pos, (uint8_t)digits[i]);
    }

    absorb_byte(state, pos, (uint8_t)'"');
    absorb_byte(state, pos, (uint8_t)'}');

    state[pos >> 3] ^= 0x01ULL << ((pos & 7) * 8);
    state[(KECCAK_RATE - 1) >> 3] ^= 0x80ULL << (((KECCAK_RATE - 1) & 7) * 8);
    keccak_f1600(state);

    for (int i = 0; i < 32; ++i) {
        out[i] = (uint8_t)((state[i >> 3] >> ((i & 7) * 8)) & 0xff);
    }
}

static __device__ __forceinline__ bool hash_meets_target(const uint8_t hash[32]) {
    for (int i = 0; i < 32; ++i) {
        if (hash[i] < C_TARGET[i]) return true;
        if (hash[i] > C_TARGET[i]) return false;
    }
    return true;
}

__global__ void mine_kernel(
    uint32_t prefix_len,
    uint64_t start_nonce,
    uint64_t total_nonces,
    unsigned long long *found_nonce,
    uint8_t *found_hash,
    int *found_flag
) {
    uint64_t index = (uint64_t)blockIdx.x * blockDim.x + threadIdx.x;
    uint64_t stride = (uint64_t)gridDim.x * blockDim.x;

    for (; index < total_nonces; index += stride) {
        if (atomicAdd(found_flag, 0) != 0) return;

        uint64_t nonce = start_nonce + index;
        uint8_t hash[32];
        keccak_json_nonce(prefix_len, nonce, hash);

        if (hash_meets_target(hash)) {
            if (atomicCAS(found_flag, 0, 1) == 0) {
                *found_nonce = (unsigned long long)nonce;
                for (int i = 0; i < 32; ++i) found_hash[i] = hash[i];
            }
            return;
        }
    }
}

extern "C" int ember_cuda_mine(
    const uint8_t *prefix,
    size_t prefix_len,
    const uint8_t *target,
    uint64_t start_nonce,
    uint64_t total_nonces,
    uint64_t *found_nonce,
    uint8_t *found_hash,
    uint64_t *checked,
    int device_id
) {
    if (prefix_len > MAX_PREFIX_LEN) return -10;
    if (cudaSetDevice(device_id) != cudaSuccess) return -1;
    if (cudaMemcpyToSymbol(C_PREFIX, prefix, prefix_len) != cudaSuccess) return -2;
    if (cudaMemcpyToSymbol(C_TARGET, target, 32) != cudaSuccess) return -3;

    unsigned long long *d_found_nonce = nullptr;
    uint8_t *d_found_hash = nullptr;
    int *d_found_flag = nullptr;

    if (cudaMalloc((void **)&d_found_nonce, sizeof(unsigned long long)) != cudaSuccess) return -4;
    if (cudaMalloc((void **)&d_found_hash, 32) != cudaSuccess) {
        cudaFree(d_found_nonce);
        return -5;
    }
    if (cudaMalloc((void **)&d_found_flag, sizeof(int)) != cudaSuccess) {
        cudaFree(d_found_nonce);
        cudaFree(d_found_hash);
        return -6;
    }

    int zero = 0;
    unsigned long long zero_nonce = 0;
    cudaMemcpy(d_found_flag, &zero, sizeof(int), cudaMemcpyHostToDevice);
    cudaMemcpy(d_found_nonce, &zero_nonce, sizeof(unsigned long long), cudaMemcpyHostToDevice);

    const int threads = EMBER_CUDA_THREADS;
    uint64_t blocks64 = (total_nonces + threads - 1) / threads;
    if (blocks64 < 1) blocks64 = 1;
    if (blocks64 > 65535ULL) blocks64 = 65535ULL;
    int blocks = (int)blocks64;

    mine_kernel<<<blocks, threads>>>(
        (uint32_t)prefix_len,
        start_nonce,
        total_nonces,
        d_found_nonce,
        d_found_hash,
        d_found_flag
    );

    cudaError_t launch_status = cudaGetLastError();
    if (launch_status != cudaSuccess) {
        cudaFree(d_found_nonce);
        cudaFree(d_found_hash);
        cudaFree(d_found_flag);
        return -7;
    }

    cudaError_t sync_status = cudaDeviceSynchronize();
    if (sync_status != cudaSuccess) {
        cudaFree(d_found_nonce);
        cudaFree(d_found_hash);
        cudaFree(d_found_flag);
        return -8;
    }

    int h_found = 0;
    unsigned long long h_nonce = 0;
    cudaMemcpy(&h_found, d_found_flag, sizeof(int), cudaMemcpyDeviceToHost);
    cudaMemcpy(&h_nonce, d_found_nonce, sizeof(unsigned long long), cudaMemcpyDeviceToHost);

    *checked = total_nonces;
    if (h_found) {
        *found_nonce = (uint64_t)h_nonce;
        cudaMemcpy(found_hash, d_found_hash, 32, cudaMemcpyDeviceToHost);
    }

    cudaFree(d_found_nonce);
    cudaFree(d_found_hash);
    cudaFree(d_found_flag);

    return h_found ? 1 : 0;
}
