import { useState } from 'react'
import { Sidebar, type NavKey } from './components/Sidebar'
import { Dashboard } from './screens/Dashboard'
import { Storage } from './screens/Storage'
import { CreateEmulator } from './screens/Create'
import { Instance, type InstanceTab } from './screens/Instance'

// Lightweight state-based router (no react-router — keeps the app dep-light).
//
// `instance` is the per-emulator hub: it replaced the global `devices` route
// (which had to ask "which emulator?" before it could show anything) and
// absorbed the old `running` route as its Display tab.
export type Route =
    | { screen: 'dashboard' }
    | { screen: 'storage' }
    | { screen: 'create' }
    | { screen: 'instance'; id: string; tab: InstanceTab }

export default function App() {
    const [route, setRoute] = useState<Route>({ screen: 'dashboard' })

    const nav: NavKey = route.screen === 'storage' ? 'storage' : 'dashboard'

    const openInstance = (id: string, tab: InstanceTab = 'display') =>
        setRoute({ screen: 'instance', id, tab })

    return (
        <div className="app">
            <Sidebar
                active={nav}
                onNavigate={(k) =>
                    setRoute(k === 'dashboard' ? { screen: 'dashboard' } : { screen: 'storage' })
                }
            />
            <main className="main">
                {route.screen === 'dashboard' && (
                    <Dashboard
                        onCreate={() => setRoute({ screen: 'create' })}
                        onOpenInstance={openInstance}
                    />
                )}
                {route.screen === 'storage' && <Storage onOpenInstance={openInstance} />}
                {route.screen === 'create' && (
                    <CreateEmulator
                        onCancel={() => setRoute({ screen: 'dashboard' })}
                        onCreated={() => setRoute({ screen: 'dashboard' })}
                    />
                )}
                {route.screen === 'instance' && (
                    <Instance
                        key={route.id}
                        id={route.id}
                        tab={route.tab}
                        onBack={() => setRoute({ screen: 'dashboard' })}
                        onTab={(tab) => setRoute({ screen: 'instance', id: route.id, tab })}
                        onDeleted={() => setRoute({ screen: 'dashboard' })}
                    />
                )}
            </main>
        </div>
    )
}
