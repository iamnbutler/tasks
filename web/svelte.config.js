import adapter from '@sveltejs/adapter-static';

/** @type {import('@sveltejs/kit').Config} */
const config = {
	kit: {
		adapter: adapter({
			pages: 'build',
			assets: 'build',
			fallback: 'index.html'
		}),
		// In dev, proxy API requests to the Rust server.
		// In production, the Rust server serves both static files and API.
	}
};

export default config;
