import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
    warmup: {
      clientFiles: ["./src/main.ts", "./src/**/*.svelte", "./src/lib/**/*.ts"]
    },
    watch: {
      ignored: [
        /(^|[\\/])\.git([\\/]|$)/,
        /(^|[\\/])\.runtime([\\/]|$)/,
        /(^|[\\/])target(?:-[^\\/]+)?([\\/]|$)/
      ]
    }
  }
});
