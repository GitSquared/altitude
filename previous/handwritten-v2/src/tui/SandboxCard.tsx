import { Box, Text, useFocus, useInput } from 'ink'
import type { SandboxHandle } from 'microsandbox'
import { attachToSandbox } from '@/isolation'
import { mountTui, unmountTui } from '.'

interface SandboxCardProps {
	sb: SandboxHandle
}

export function SandboxCard({ sb }: SandboxCardProps) {
	const { isFocused } = useFocus()

	useInput(
		(_, key) => {
			if (key.return) {
				handControlToSandbox(sb)
			}

			if (key.delete || key.backspace) {
				void sb.destroy()
			}
		},
		{
			isActive: isFocused,
		},
	)

	return (
		<Box
			flexDirection="column"
			borderStyle="round"
			borderDimColor={!isFocused}
			padding={1}
		>
			<Text bold>{sb.id}</Text>
			<Text dimColor>{sb.name}</Text>
			<Text>{sb.status}</Text>
		</Box>
	)
}

async function handControlToSandbox(sb: SandboxHandle) {
	await unmountTui()
	await attachToSandbox(sb.name)
	mountTui()
}
