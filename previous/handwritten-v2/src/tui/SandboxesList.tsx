import { Box, Text } from 'ink'
import { useEffect, useState } from 'react'
import { listAllSandboxes } from '@/isolation'
import { SandboxCard } from './SandboxCard'

const pollingInterval = 1_000

export function SandboxesList() {
	const [sandboxes, setSandboxes] = useState<
		Awaited<ReturnType<typeof listAllSandboxes>>
	>([])

	useEffect(() => {
		const polling = setInterval(async () => {
			const newSandboxes = await listAllSandboxes()
			setSandboxes(newSandboxes)
		}, pollingInterval)
		return () => clearInterval(polling)
	}, [])

	return (
		<Box flexDirection="row" gap={1} flexWrap="wrap" overflow="hidden">
			{sandboxes.length === 0 ? (
				<Text dimColor>No sandboxes running</Text>
			) : (
				sandboxes.map((sb) => <SandboxCard key={sb.id} sb={sb} />)
			)}
		</Box>
	)
}
