// Restrict the Gmail integration to read-only operations.
// Add narrower scopes here per-feature when modify/send is required.
export const GMAIL_READ_SCOPES = [
  "https://www.googleapis.com/auth/gmail.readonly",
  "https://www.googleapis.com/auth/userinfo.email",
];
