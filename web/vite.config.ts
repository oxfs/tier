import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

export default defineConfig({
  plugins: [vue()],
  server: {
    proxy: {
      "/api": {
        target: "http://127.0.0.1:9480",
        changeOrigin: true,
        configure(proxy) {
          proxy.on("proxyReq", (outgoing, request) => {
            if (request.method !== "POST") return;
            const host = request.headers.host ?? "";
            const local = /^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$/.test(
              host,
            );
            const trusted =
              local && request.headers.origin === "http://" + host;
            outgoing.setHeader(
              "Origin",
              trusted ? "http://127.0.0.1:9480" : "null",
            );
          });
        },
      },
    },
  },
  build: {
    rollupOptions: {
      output: {
        entryFileNames: "assets/app.js",
        assetFileNames: "assets/app[extname]",
      },
    },
  },
});
