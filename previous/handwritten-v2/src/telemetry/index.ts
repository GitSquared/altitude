const logFile = 'debug.log'
const logFileEnabled =
	Bun.env.NODE_ENV === 'development' || Bun.env.NODE_ENV === 'test'

const logFileWriter = logFileEnabled
	? Bun.file(logFile).writer({ highWaterMark: 126 })
	: null

export function log(msg: string) {
	if (!logFileWriter) return

	const timestamp = new Date().toISOString()
	const logLine = `[${timestamp}]: ${msg}`

	console.log(logLine)
	logFileWriter.write(`${logLine}\n`)
}
