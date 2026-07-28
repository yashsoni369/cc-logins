import path from "node:path";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    restoreMocks: true,
    // `.worktrees/` holds local git worktrees, which carry their own copy of
    // src/ and node_modules. Without this, `pnpm test` collects every checkout
    // at once and reports failures from a stale branch as if they were yours.
    // CI never noticed because a fresh clone has no worktrees.
    exclude: ["**/node_modules/**", "**/dist/**", "**/.worktrees/**"],
  },
});
