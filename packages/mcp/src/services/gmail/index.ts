export { GMAIL_READ_SCOPES } from "./scopes";
export {
  createOAuth2Client,
  getAuthUrl,
  exchangeCodeAndPersist,
  persistTokens,
} from "./auth";
export { getOAuth2ClientForAccount, getGmailClient } from "./client";
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
export { startGmailPoller } from "./poller";
