import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 前端构建：静态产物输出 dist/，dev 端口 5420（与官方一致）
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5420,
    strictPort: true,
  },
  build: {
    target: "chrome105",
    outDir: "dist",
    assetsInlineLimit: 0,
  },
});
