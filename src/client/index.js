import { SingleBar, Presets } from "cli-progress";
import WebTorrent from "webtorrent";
import fs from "fs";
import path from "path";

import { LOGGER , LOADERS, FORMATTERS, VALIDATORS } from '../utils/index.js';

const { log, error } = LOGGER;
const { updateSpinner } = LOADERS;
const { formatETA, formatSize, formatSpeed, getDisplayFormat } = FORMATTERS;
const { isValidMagnetURL, isValidDownloadURL } = VALIDATORS;

const DOWNLOAD_DIRECTORY = './downloads';
const PART_COUNT = 4;

const getFilename = (downloadURL, headers) => {
    const disposition = headers.get('content-disposition') || '';
    const dispositionMatch = disposition.match(/filename\*?=(?:UTF-8''|"?)([^";]+)/i);
    const urlFilename = path.basename(new URL(downloadURL).pathname);
    const filename = dispositionMatch?.[1] || urlFilename || 'download.bin';
    return decodeURIComponent(filename.replace(/^"|"$/g, '')).replace(/[<>:"/\\|?*]/g, '_');
};

const updateURLProgress = (progressBar, state, total) => {
    const elapsedSeconds = (Date.now() - state.startedAt) / 1000;
    const speed = elapsedSeconds > 0 ? state.downloaded / elapsedSeconds : 0;
    const remaining = speed > 0 ? ((total - state.downloaded) / speed) * 1000 : Infinity;
    progressBar.update(state.downloaded, {
        have: formatSize(state.downloaded),
        fullSize: formatSize(total),
        estimate: formatETA(remaining),
        numPeers: 0,
        speed: formatSpeed(speed)
    });
};

const streamResponse = async (response, fileHandle, position, state, progressBar, total) => {
    let writePosition = position;
    for await (const chunk of response.body) {
        await fileHandle.write(chunk, 0, chunk.length, writePosition);
        writePosition += chunk.length;
        state.downloaded += chunk.length;
        updateURLProgress(progressBar, state, total);
    }
};

const getRemoteMetadata = async (downloadURL) => {
    try {
        const response = await fetch(downloadURL, { method: 'HEAD' });
        if (!response.ok) return null;
        return {
            size: Number(response.headers.get('content-length')),
            supportsRanges: response.headers.get('accept-ranges')?.toLowerCase() === 'bytes',
            filename: getFilename(downloadURL, response.headers)
        };
    } catch {
        return null;
    }
};

export const startURLDownload = async (downloadURL) => {
    if (!isValidDownloadURL(downloadURL)) return error('\nInvalid HTTP(S) URL!');

    let progressBar;
    let fileHandle;
    try {
        log('\nPreparing direct download...\n');
        const metadata = await getRemoteMetadata(downloadURL);
        const response = metadata?.supportsRanges && Number.isFinite(metadata.size) && metadata.size > 0
            ? null
            : await fetch(downloadURL);

        if (response && !response.ok) throw new Error(`Download request failed (${response.status})`);

        const total = metadata?.size > 0 ? metadata.size : Number(response?.headers.get('content-length')) || 0;
        const filename = metadata?.filename || getFilename(downloadURL, response.headers);
        await fs.promises.mkdir(DOWNLOAD_DIRECTORY, { recursive: true });
        fileHandle = await fs.promises.open(path.join(DOWNLOAD_DIRECTORY, filename), 'w');
        progressBar = new SingleBar({ format: getDisplayFormat() }, Presets.shades_classic);
        const state = { downloaded: 0, startedAt: Date.now() };

        if (metadata?.supportsRanges && total > 0) {
            await fileHandle.truncate(total);
            progressBar.start(total, 0);
            const partSize = Math.ceil(total / PART_COUNT);
            await Promise.all(Array.from({ length: PART_COUNT }, async (_, partIndex) => {
                const start = partIndex * partSize;
                const end = Math.min(total - 1, start + partSize - 1);
                if (start > end) return;
                const partResponse = await fetch(downloadURL, { headers: { Range: `bytes=${start}-${end}` } });
                if (partResponse.status !== 206) throw new Error('The server did not honor ranged requests');
                await streamResponse(partResponse, fileHandle, start, state, progressBar, total);
            }));
        } else {
            progressBar.start(total || 1, 0);
            await streamResponse(response, fileHandle, 0, state, progressBar, total || state.downloaded);
        }

        progressBar.update(total || state.downloaded);
        progressBar.stop();
        log(`\nDirect download finished: ${filename}`);
    } catch (downloadError) {
        progressBar?.stop();
        error(`\nDirect download failed: ${downloadError.message}`);
    } finally {
        await fileHandle?.close();
    }
};

export const startDownload = (magnetURI) => {

    //EARLY EXIT
    if (!isValidMagnetURL(magnetURI)) return error('\nInvalid magnet URL !\nPlease update the link in server.js file.');
    log('\n');
    let memoryRef = setInterval(()=>updateSpinner('Initializing torrent client & verifying data...'),50);
    const client = new WebTorrent();

    // Add a torrent (replace with your magnet link)
    client.add(magnetURI, { path: './downloads' }, (torrent) => {
        clearInterval(memoryRef);
        log('\n\nTorrent is downloading...\n');
        log('Progress\n');

        const singleBar = new SingleBar({
            format: getDisplayFormat()
        }, Presets.shades_classic);


        singleBar.start(100, 0); // start progress

        torrent.on('download', () => {

            const { timeRemaining, downloadSpeed, length, downloaded, progress, numPeers } = torrent;

            // formatted data
            const have = formatSize(downloaded);
            const fullSize = formatSize(length);
            const estimate = formatETA(timeRemaining);
            const speed = formatSpeed(downloadSpeed);

            // Update the progress bar with progress, ETA, and speed
            singleBar.update(progress * 100, {
                have,
                fullSize,
                estimate,
                numPeers,
                speed
            });

            // Check if the download is complete
            if(progress === 1){
                singleBar.stop();
                log('\nTorrent download finished !');
                process.exit(0);
            }

        });
        torrent.on('error', (err) => error(err));
    });
}
