use std::time::Duration;

pub(super) const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const WEBHOOK_MAX_AGE: Duration = Duration::from_secs(300);
pub(super) const AGENT_SESSION_EVENT: &str = "AgentSessionEvent";
pub(super) const VIEWER_QUERY: &str = "query Viewer { viewer { id app organization { id } } }";

pub(super) const ISSUE_QUERY: &str = "query Issue($id: String!) { issue(id: $id) { \
    id identifier title description url project { name } \
    team { id key name organization { id } } } }";

pub(super) const ISSUE_COMMENTS_QUERY: &str = "query IssueComments($id: String!, $after: String) { \
    issue(id: $id) { comments(first: 100, after: $after, orderBy: createdAt) { \
    nodes { id body createdAt user { displayName name } } pageInfo { hasNextPage endCursor } } } }";

pub(super) const ISSUE_DELEGATION_QUERY: &str = "query IssueDelegation($id: String!) { \
    issue(id: $id) { id delegate { id } state { id name type } \
    team { id organization { id } } } }";

// Issues the app has been asked to work on. A delegation is delivered by webhook,
// and a webhook that arrives while this machine is restarting is gone: this is how
// the machine finds out what it was asked to do while it was not listening.
// Only work that is still work. An issue that reached a completed or cancelled state
// keeps its delegation, and picking one of those up on the next start is starting
// something somebody already finished.
pub(super) const DELEGATED_ISSUES_QUERY: &str = "query DelegatedIssues($app: ID!) { \
    issues(filter: { delegate: { id: { eq: $app } }, \
                    state: { type: { nin: [\"completed\", \"canceled\"] } } }, first: 25) { \
    nodes { id identifier title url project { name } \
    agentSessions(first: 10) { nodes { id createdAt } } \
    team { id key name organization { id } } } } }";

pub(super) const ISSUE_STATES_QUERY: &str = "query IssueStates($id: String!) { \
    issue(id: $id) { id team { id organization { id } \
    states(first: 250) { nodes { id name type position } } } } }";

pub(super) const CREATE_ACTIVITY_MUTATION: &str = "mutation CreateAgentActivity($input: AgentActivityCreateInput!) { \
    agentActivityCreate(input: $input) { success agentActivity { id } } }";

// The input type is named for the mutation, not for the input: Linear has no
// `AgentSessionCreateOnIssueInput`, and asking for one is a validation error
// rather than a refusal, which is why the session was never created.
pub(super) const FILE_UPLOAD_MUTATION: &str = "mutation Upload($filename: String!, $size: Int!, $contentType: String!) { \
    fileUpload(filename: $filename, size: $size, contentType: $contentType) { \
    uploadFile { uploadUrl assetUrl headers { key value } } } }";

pub(super) const CREATE_SESSION_ON_ISSUE_MUTATION: &str = "mutation CreateAgentSessionOnIssue($input: AgentSessionCreateOnIssue!) { \
    agentSessionCreateOnIssue(input: $input) { success agentSession { id } } }";

pub(super) const UPDATE_STATE_MUTATION: &str = "mutation UpdateIssueState($id: String!, $input: IssueUpdateInput!) { \
    issueUpdate(id: $id, input: $input) { success } }";

pub(super) const COMMENT_SITE_QUERY: &str = "query CommentSite($id: String!) { comment(id: $id) { \
    id body parentId issueId documentContentId isArtificialAgentSessionRoot \
    user { id name } \
    parent { id parentId issueId documentContentId user { id } } \
    issue { id team { id } comments(first: 100) { pageInfo { hasNextPage } \
    nodes { user { id } } } } \
    documentContent { id document { id creator { id } issue { id } \
    comments(first: 100) { pageInfo { hasNextPage } nodes { user { id } } } } } } }";

pub(super) const CREATE_COMMENT_MUTATION: &str = "mutation CreateComment($input: CommentCreateInput!) { \
    commentCreate(input: $input) { success comment { id } } }";

pub(super) const COMMENT_EXISTS_QUERY: &str = "query CommentExists($id: String!) { comment(id: $id) { \
    id parentId body resolvedAt } }";

pub(super) const COMMENT_UPDATE_MUTATION: &str = "mutation UpdateComment($id: String!, $input: CommentUpdateInput!) { \
    commentUpdate(id: $id, input: $input) { success } }";

pub(super) const COMMENT_UNRESOLVE_MUTATION: &str = "mutation UnresolveComment($id: String!) { \
    commentUnresolve(id: $id) { success } }";

pub(super) const COMMENT_RESOLVE_MUTATION: &str = "mutation ResolveComment($id: String!, $resolvingCommentId: String) { \
    commentResolve(id: $id, resolvingCommentId: $resolvingCommentId) { success } }";

pub(super) const CREATE_REACTION_MUTATION: &str = "mutation CreateReaction($input: ReactionCreateInput!) { \
    reactionCreate(input: $input) { success reaction { id } } }";

pub(super) const USER_URL_QUERY: &str =
    "query UserUrl($id: String!) { user(id: $id) { id name url } }";

pub(super) const UPDATE_SESSION_SUMMARY_MUTATION: &str = "mutation UpdateSession($id: String!, $input: AgentSessionUpdateInput!) { \
    agentSessionUpdate(id: $id, input: $input) { success } }";
