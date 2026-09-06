import { type Instance, render } from 'ink'
import { AltitudeHome } from './Home'

const isDev = Bun.env.NODE_ENV === 'development'

let currentInstance: Instance | null = null

export function mountTui(): Instance {
	currentInstance = render(<AltitudeHome />, {
		maxFps: 60,
		concurrent: true,
		incrementalRendering: !isDev,
		alternateScreen: !isDev,
		patchConsole: !isDev,
	})

	return currentInstance
}

export async function unmountTui() {
	if (!currentInstance) throw new Error('TUI is not mounted')

	currentInstance.clear()
	currentInstance.unmount()
	await currentInstance.waitUntilExit()
	currentInstance = null
}
