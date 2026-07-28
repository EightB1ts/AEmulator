export function fmtBytes(n: number): string {
    if (n <= 0) return '0 B'
    const u = ['B', 'KB', 'MB', 'GB', 'TB']
    const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)))
    return `${(n / 1024 ** i).toFixed(i === 0 ? 0 : 1)} ${u[i]}`
}

export function fmtMb(mb: number): string {
    return mb >= 1024 ? `${(mb / 1024).toFixed(mb % 1024 === 0 ? 0 : 1)} GB` : `${mb} MB`
}
