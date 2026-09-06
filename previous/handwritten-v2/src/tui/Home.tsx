import { Box, Text, useApp, useInput, useWindowSize } from 'ink'
import { Suspense } from 'react'
import { version } from '@/../package.json'
import { killAllSandboxes, startIsolatedSandbox } from '@/isolation'
import { IsolationEngineStatus } from './IsolationEngineStatus'
import { Loading } from './Loading'
import { SandboxesList } from './SandboxesList'

export function AltitudeHome() {
	const { exit } = useApp()

	const { columns, rows } = useWindowSize()

	useInput((input) => {
		if (input === 'q') void killAllSandboxes().then(() => exit())

		if (input === 'c') void startIsolatedSandbox()
	})

	return (
		<Box
			width={columns}
			height={rows}
			flexDirection="column"
			justifyContent="space-between"
			alignItems="stretch"
			gap={1}
			padding={1}
		>
			<Box paddingX={1} flexDirection="row" justifyContent="space-between">
				<Text bold color="cyan">
					{'⛰  '}altitude
				</Text>
				<Text dimColor>v{version}</Text>
			</Box>
			<Box padding={1} borderStyle="round" flexGrow={1}>
				<SandboxesList />
			</Box>
			<Box paddingX={1} flexDirection="row" justifyContent="space-between">
				<Suspense fallback={<Loading />}>
					<IsolationEngineStatus />
				</Suspense>
				<Text dimColor>
					press c to spawn a new sandbox, enter to attach to selected sandbox,
					del to destroy it, q to quit Altitude
				</Text>
			</Box>
		</Box>
	)
}
