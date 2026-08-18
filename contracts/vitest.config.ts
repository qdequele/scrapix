import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    include: ["tests/**/*.contract.test.ts"],
    // Contract tests hit a live backend; run files sequentially so
    // per-file accounts don't race on shared infrastructure.
    fileParallelism: false,
    testTimeout: 15_000,
    hookTimeout: 30_000,
  },
});
