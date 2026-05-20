import { defineConfig } from "vite";
import tailwindcss from "@tailwindcss/vite";

// Dev proxy: 把 /v1/*, /healthz, /install.sh, /capabilities 转给已经部署的
// 新机器 veda-server。生产环境下走 nginx 同源反代，无 CORS。
const upstream = process.env.VEDA_UPSTREAM || "http://10.79.55.89:3000";

export default defineConfig({
  plugins: [tailwindcss()],
  server: {
    proxy: {
      "/v1": { target: upstream, changeOrigin: false },
      "/healthz": { target: upstream, changeOrigin: false },
      "/install.sh": { target: upstream, changeOrigin: false },
      "/capabilities": { target: upstream, changeOrigin: false },
    },
  },
});
