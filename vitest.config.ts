import path from "node:path";
import react from "@vitejs/plugin-react";
import { configDefaults, defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    restoreMocks: true,
    // Git worktrees may live under `.worktrees/` while developing. They are
    // separate checkouts with their own React dependency graph, not tests
    // belonging to this checkout.
    exclude: [...configDefaults.exclude, "**/.worktrees/**"],
  },
});
