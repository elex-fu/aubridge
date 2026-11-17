import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// 产物直接落在 tauri.conf.json 的 frontendDist(crates/smoke/ui):
// 中间不引入 dist 目录,cargo 侧无需知道前端工具链存在。
export default defineConfig({
  plugins: [react()],
  build: { outDir: "../ui", emptyOutDir: true },
});
