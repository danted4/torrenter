import { startDownload, startURLDownload } from "./src/client/index.js";
import readline from 'readline';
import { LOGGER, VALIDATORS } from "./src/utils/index.js";


const r1 = readline.createInterface(({
    input: process.stdin,
    output: process.stdout
}))

const promptUser = () => {
    r1.question('\nChoose download type:\n1. Magnet URL\n2. Direct URL (multi-part)\n\n', (choice) => {
        if (choice.trim() === '2') {
            r1.question('\nPlease paste the direct download URL:\n\n', (downloadURL) => {
                if (!VALIDATORS.isValidDownloadURL(downloadURL.trim())) {
                    LOGGER.error('\nInvalid HTTP(S) URL! Try again ...');
                    return promptUser();
                }
                startURLDownload(downloadURL.trim());
                r1.close();
            });
            return;
        }

        r1.question('\nPlease paste your magnetURI:\n\n', (magnetURI) => {
            if(!VALIDATORS.isValidMagnetURL(magnetURI.trim())) {
                LOGGER.error('\nInvalid magnet URL! Try again ...');
                return promptUser();
            }
            startDownload(magnetURI.trim());
            r1.close();
        });
    });
}

promptUser();
