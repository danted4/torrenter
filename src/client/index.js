import { SingleBar, Presets } from "cli-progress";
import WebTorrent from "webtorrent";
import fs from "fs";
import path from "path";
import { createHash } from "crypto";

import { LOGGER , LOADERS, FORMATTERS, VALIDATORS } from '../utils/index.js';

const { log, error } = LOGGER;
const { updateSpinner } = LOADERS;
const { formatETA, formatSize, formatSpeed, getDisplayFormat } = FORMATTERS;
const { isValidMagnetURL, isValidDownloadURL } = VALIDATORS;

const DOWNLOAD_DIRECTORY = './downloads';
const RESUME_DIRECTORY = path.join(DOWNLOAD_DIRECTORY, '.nodewt');
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

const getResumeKey = (downloadURL) => createHash('sha256').update(downloadURL).digest('hex').slice(0, 16);

const getResumeManifest = async (resumeKey) => {
    try {
        return JSON.parse(await fs.promises.readFile(path.join(RESUME_DIRECTORY, `${resumeKey}.json`), 'utf8'));
    } catch {
        return null;
    }
};

const saveResumeManifest = (resumeKey, manifest) => fs.promises.writeFile(
    path.join(RESUME_DIRECTORY, `${resumeKey}.json`),
    JSON.stringify(manifest)
);

const removeResumeFiles = async (resumeKey) => {
    await Promise.all([
        fs.promises.rm(path.join(RESUME_DIRECTORY, `${resumeKey}.json`), { force: true }),
        ...Array.from({ length: PART_COUNT }, (_, partIndex) => fs.promises.rm(
            path.join(RESUME_DIRECTORY, `${resumeKey}.part-${partIndex}`),
            { force: true }
        ))
    ]);
};

const mergeParts = async (partPaths, outputHandle, progressBar, state, total) => {
    for (const partPath of partPaths) {
        const partHandle = await fs.promises.open(partPath, 'r');
        try {
            let position = 0;
            while (true) {
                const buffer = Buffer.allocUnsafe(1024 * 1024);
                const { bytesRead } = await partHandle.read(buffer, 0, buffer.length, position);
                if (!bytesRead) break;
                await outputHandle.write(buffer, 0, bytesRead, state.merged);
                position += bytesRead;
                state.merged += bytesRead;
                updateURLProgress(progressBar, { downloaded: state.merged, startedAt: state.startedAt }, total);
            }
        } finally {
            await partHandle.close();
        }
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
        const response = Number.isFinite(metadata?.size) && metadata.size > 0
            ? null
            : await fetch(downloadURL);

        if (response && !response.ok) throw new Error(`Download request failed (${response.status})`);

        const total = metadata?.size > 0 ? metadata.size : Number(response?.headers.get('content-length')) || 0;
        const filename = metadata?.filename || getFilename(downloadURL, response.headers);
        await fs.promises.mkdir(DOWNLOAD_DIRECTORY, { recursive: true });
        progressBar = new SingleBar({ format: getDisplayFormat() }, Presets.shades_classic);
        const state = { downloaded: 0, merged: 0, startedAt: Date.now() };

        if (total > 0) {
            const resumeKey = getResumeKey(downloadURL);
            const partSize = Math.ceil(total / PART_COUNT);
            const manifest = await getResumeManifest(resumeKey);
            const canResume = manifest?.url === downloadURL
                && manifest.size === total
                && manifest.partCount === PART_COUNT;
            if (!canResume) await removeResumeFiles(resumeKey);
            const nextManifest = { url: downloadURL, size: total, partCount: PART_COUNT, filename };
            await saveResumeManifest(resumeKey, nextManifest);
            const partPaths = Array.from({ length: PART_COUNT }, (_, partIndex) =>
                path.join(RESUME_DIRECTORY, `${resumeKey}.part-${partIndex}`)
            );
            const existingSizes = await Promise.all(partPaths.map(async (partPath, partIndex) => {
                try {
                    const size = (await fs.promises.stat(partPath)).size;
                    const partEnd = Math.min(total, (partIndex + 1) * partSize);
                    return Math.min(size, Math.max(0, partEnd - partIndex * partSize));
                } catch {
                    return 0;
                }
            }));
            state.downloaded = existingSizes.reduce((sum, size) => sum + size, 0);
            progressBar.start(total, state.downloaded);
            let multipartCompleted = true;
            try {
                await Promise.all(partPaths.map(async (partPath, partIndex) => {
                    const start = partIndex * partSize;
                    const end = Math.min(total - 1, start + partSize - 1);
                    if (start > end) return;
                    const existingSize = existingSizes[partIndex];
                    if (existingSize === end - start + 1) return;
                    const rangeStart = start + existingSize;
                    const partResponse = await fetch(downloadURL, { headers: { Range: `bytes=${rangeStart}-${end}` } });
                    if (partResponse.status !== 206) throw new Error('The server did not honor ranged requests');
                    const partHandle = await fs.promises.open(partPath, existingSize ? 'r+' : 'w');
                    try {
                        await streamResponse(partResponse, partHandle, existingSize, state, progressBar, total);
                    } finally {
                        await partHandle.close();
                    }
                    const completedSize = (await fs.promises.stat(partPath)).size;
                    if (completedSize !== end - start + 1) throw new Error('A download part was incomplete');
                }));
            } catch (rangeError) {
                if (rangeError.message !== 'The server did not honor ranged requests') throw rangeError;
                multipartCompleted = false;
                await removeResumeFiles(resumeKey);
                state.downloaded = 0;
                const fallbackResponse = await fetch(downloadURL);
                if (!fallbackResponse.ok) throw rangeError;
                fileHandle = await fs.promises.open(path.join(DOWNLOAD_DIRECTORY, filename), 'w');
                await streamResponse(fallbackResponse, fileHandle, 0, state, progressBar, total);
                await fileHandle.close();
                fileHandle = null;
            }
            if (multipartCompleted) {
                fileHandle = await fs.promises.open(path.join(DOWNLOAD_DIRECTORY, filename), 'w');
                await mergeParts(partPaths, fileHandle, progressBar, state, total);
                await fileHandle.close();
                fileHandle = null;
            }
            await removeResumeFiles(resumeKey);
        } else {
            fileHandle = await fs.promises.open(path.join(DOWNLOAD_DIRECTORY, filename), 'w');
            progressBar.start(total || 1, 0);
            await streamResponse(response, fileHandle, 0, state, progressBar, total || state.downloaded);
            await fileHandle.close();
            fileHandle = null;
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
