import { Text } from 'ink'
import { use } from 'react'
import { setupIsolationEngine } from '@/isolation'

let cachedPromise: Promise<true> | null = null

function getCachedSetupIsolationEngineCall() {
	if (!cachedPromise) cachedPromise = setupIsolationEngine()
	return cachedPromise
}

export function IsolationEngineStatus() {
	const status = use(getCachedSetupIsolationEngineCall())

	return (
		<Text dimColor>Isolation engine is {status ? 'ready' : 'not ready'}</Text>
	)
}
