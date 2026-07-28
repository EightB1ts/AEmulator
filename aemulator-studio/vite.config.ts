import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Vite config for the Tauri desktop shell. Tauri runs `beforeDevCommand`
// (`pnpm dev`) and connects to this dev server at `devUrl`
// (http://localhost:1420). `clearScreen: false` keeps Rust compiler output
// visible; `strictPort` fails loudly on a port collision instead of drifting
// off the URL Tauri expects.
export default defineConfig({
    plugins: [react()],
    clearScreen: false,
    server: {
        port: 1420,
        strictPort: true,
    },
    build: {
        // macOS WKWebView baseline — Tauri targets a modern Safari engine.
        target: 'safari15',
        outDir: 'dist',
        emptyOutDir: true,
    },
})
