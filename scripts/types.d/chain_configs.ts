import {readArtifact} from "../helpers.js";

export const chainConfigs: Config = readArtifact(`${process.env.CHAIN_ID}`, 'chain_configs');