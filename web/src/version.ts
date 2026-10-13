// This page's build. build.sh defines the two constants from package.json
// and git.

import { PROTOCOL, type BuildInfo } from './protocol';

declare const TQ_WEB_VERSION: string;
declare const TQ_GIT_SHA: string;

export const WEB_BUILD: BuildInfo = {
  component: 'tq-web',
  version: TQ_WEB_VERSION,
  git_sha: TQ_GIT_SHA,
  protocol: PROTOCOL,
};
