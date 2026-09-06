import { defaultTheme, extendTheme, Spinner, ThemeProvider } from '@inkjs/ui'
import type { TextProps } from 'ink'

const customTheme = extendTheme(defaultTheme, {
	components: {
		Spinner: {
			styles: {
				frame: (): TextProps => ({
					color: 'cyan',
				}),
				label: (): TextProps => ({
					dimColor: true,
				}),
			},
		},
	},
})

export function Loading() {
	return (
		<ThemeProvider theme={customTheme}>
			<Spinner type="dots" label="Loading..." />
		</ThemeProvider>
	)
}
