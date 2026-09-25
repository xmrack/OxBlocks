/* Prints fixtures/fcmp/membership_proof_size.txt's table, for every input
 * and layer count, from monerod's own FCMP++ library. Build against a
 * stressnet monerod build directory:
 *
 *   cc tools/membership-proof-size.c \
 *     <build>/src/fcmp_pp/fcmp_pp_rust/libfcmp_pp_rust.a -lpthread -ldl -lm
 */
#include <stdint.h>
#include <stdio.h>

uintptr_t membership_proof_size(uintptr_t n_inputs, uintptr_t n_tree_layers);

int main(void) {
  /* Every count consensus allows: FCMP_PLUS_PLUS_MAX_INPUTS and
   * FCMP_PLUS_PLUS_MAX_LAYERS in src/cryptonote_config.h. */
  for (unsigned i = 1; i <= 128; i++)
    for (unsigned l = 1; l <= 12; l++)
      printf("%u %u %lu\n", i, l, (unsigned long) membership_proof_size(i, l));
  return 0;
}
