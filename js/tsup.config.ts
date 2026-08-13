import { defineConfig } from 'tsup';

export default defineConfig({
  entry: {
    index: 'src/index.ts',
    nest: 'src/nest/index.ts',
    middleware: 'src/middleware/index.ts',
  },
  format: ['esm', 'cjs'],
  dts: true,
  sourcemap: true,
  clean: true,
  target: 'node22',
  // Without this, esbuild inlines shared core code (AdminGuardError, the
  // circuit breaker, etc.) separately into each of the three CJS entry
  // bundles, producing three unrelated class definitions with the same name
  // — `instanceof` checks then fail across entry points even though the
  // consumer only ever required "@maxion/admin-guard". Splitting forces a
  // shared chunk so every entry point requires the SAME module instance.
  splitting: true,
  external: ['@nestjs/common', '@nestjs/core'],
});
