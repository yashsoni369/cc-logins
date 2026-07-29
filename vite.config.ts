import path from "node:path";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri sets TAURI_DEV_HOST when developing against a physical device; on the
// desktop it is unset and the server binds to localhost, which answers on both
// IPv4 and IPv6. Do NOT pass `--host 127.0.0.1` on the command line: that binds
// IPv4 only, and Chrome on Windows resolves `localhost` to ::1 first, so the
// page fails to load even though the server is up.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  // Relative asset URLs. Tauri serves the bundle from a custom protocol, not
  // from a web root, so absolute "/assets/..." paths 404 in the packaged app.
  base: "./",
  resolve: {
    // Must mirror the "@/*" paths entry in tsconfig.json — tsc resolves that
    // alias on its own, but vite does not, so a build breaks without this.
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  // Tauri owns the terminal; clearing it hides cargo's output.
  clearScreen: false,
  server: {
    port: 1420,
    // Tauri's devUrl points at this exact port. If it is taken, fail loudly
    // rather than silently moving to 1421 and leaving the window blank.
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // src-tauri is Rust; cargo watches it, vite must not.
      ignored: ["**/src-tauri/**"],
    },
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  build: {
    // Match the webviews Tauri actually ships against.
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari13",
    // oxc (Vite 8's default), not esbuild: esbuild cannot lower destructuring to
    // the safari13 target, so the macOS and Linux builds fail under it.
    minify: process.env.TAURI_ENV_DEBUG ? false : "oxc",
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    rollupOptions: {
      // Two windows, two HTML entries: the main dashboard and the tray
      // popover (see `tauri.conf.json`'s "popover" window, which points its
      // `url` at popover.html). Both must be emitted into `dist/` for Tauri
      // to serve either one.
      input: {
        main: path.resolve(__dirname, "index.html"),
        popover: path.resolve(__dirname, "popover.html"),
      },
    },
  },
});
