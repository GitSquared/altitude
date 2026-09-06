import envPaths from 'env-paths'
import {
	Image,
	isInstalled,
	Sandbox,
	type SandboxHandle,
	setup,
} from 'microsandbox'
import { name } from '@/../package.json'
import { log } from '@/telemetry'

const BASE_IMAGE = 'alpine'

let ranInstall: boolean | Promise<boolean> = false

export async function setupIsolationEngine(): Promise<boolean> {
	if (ranInstall !== false) return ranInstall === true ? true : ranInstall

	if (isInstalled()) {
		ranInstall = true
		return ranInstall
	}

	ranInstall = new Promise((resolve, reject) => {
		const dataPath = envPaths(name, { suffix: '' }).data

		log(`Setting up isolation engine at ${dataPath}...`)
		setup()
			.baseDir(dataPath)
			.install()
			.then(async () => {
				log('Pulling base image...')

				const sandbox = await Sandbox.builder('altitude-base-image-pull')
					.image(BASE_IMAGE)
					.pullPolicy('always')
					.disableNetwork()
					.ephemeral(true)
					.replaceWithTimeout(0)
					.create()

				await Image.prune()

				await sandbox.destroy()

				log('Isolation engine is ready')

				ranInstall = true
				resolve(ranInstall)
			})
			.catch((err) => reject(err))
	})

	return ranInstall
}

export async function startIsolatedSandbox(): Promise<Sandbox> {
	const id = `altitude-${Bun.randomUUIDv7()}`

	log(`Starting isolated sandbox ${id}...`)

	const sb = await Sandbox.builder(id)
		.image(BASE_IMAGE)
		.pullPolicy('never')
		.disableNetwork()
		.label('altitude', 'true')
		.replaceWithTimeout(0)
		.create()

	return sb
}

export async function listAllSandboxes(): Promise<SandboxHandle[]> {
	return Sandbox.list().then((page) => page.sandboxes)
}

export async function attachToSandbox(id: string) {
	const sb = await Sandbox.get(id)
	if (!sb) throw new Error(`Sandbox with id ${id} not found`)

	await sb
		.connectOrStart()
		.then((sandbox) => sandbox.attachWith('sh', (b) => b.detachKeys('ctrl-d')))
}

export async function killAllSandboxes(): Promise<void> {
	await Sandbox.list().then((page) =>
		Promise.all(
			page.sandboxes.map((sb) =>
				sb.destroy({
					force: true,
					timeoutMs: 2_000,
				}),
			),
		),
	)
}
