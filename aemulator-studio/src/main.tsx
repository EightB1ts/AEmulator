import React from 'react'
import ReactDOM from 'react-dom/client'
import App from './App'
import './index.css'

// Match the OS light/dark theme on load and keep it live after first paint.
try {
    const mq = window.matchMedia('(prefers-color-scheme: dark)')
    document.documentElement.classList.toggle('dark', mq.matches)
    mq.addEventListener('change', (e) => {
        document.documentElement.classList.toggle('dark', e.matches)
    })
} catch {
    /* non-browser env */
}

ReactDOM.createRoot(document.getElementById('root') as HTMLElement).render(
    <React.StrictMode>
        <App />
    </React.StrictMode>,
)
