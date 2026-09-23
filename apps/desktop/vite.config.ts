import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: { host: "127.0.0.1", port: 1420, strictPort: true },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
    setupFiles: ["./src/test/setup.ts"],
    // The thread's behavior is proven against the DOM, not against a running
    // backend: no Tauri runtime is available under jsdom.
    restoreMocks: true,
  },
});
