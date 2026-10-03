export { GMAIL_READ_SCOPES } from "./scopes";
export { createOAuth2Client, getAuthUrl, exchangeCodeAndPersist } from "./auth";
export { createGmailModule, type GmailModule } from "./client";
export { createGmailStore, type GmailStore, type GmailAccount } from "./storage";
export {
  listMessages,
  searchMessages,
  getMessage,
  getRawMessage,
  type ListMessagesOptions,
  type MessageSummary,
  type MessageDetail,
  type PageResult,
} from "./messages";
export {
  findAttachments,
  fetchAttachmentData,
  type AttachmentRef,
} from "./attachments";
export { startGmailPoller, type GmailPollerDeps } from "./poller";
