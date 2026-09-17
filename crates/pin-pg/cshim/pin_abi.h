#ifndef PIN_ABI_H
#define PIN_ABI_H

#include <stdint.h>

// these scalar probes take no pointers, allocate nothing and cannot raise error.
uint64_t pin_abi_constant(uint32_t key);
uint64_t pin_abi_am_offset(uint32_t key);

#endif
