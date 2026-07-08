//! In-process integration tests for the pg_graphql-compatible `/graphql/v1`
//! endpoint: introspection, collections (filter / orderBy / keyset
//! pagination / totalCount), node lookup, FK relationships, mutations with
//! `atMost` rollback, RLS through GraphQL, and document features (variables,
//! fragments, aliases, @skip).

#![cfg(feature = "supabase")]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use guardian_db::sql::MemoryStorage;
use guardian_db::sql::engine::{Database, Session};
use guardian_db::supabase::project::ProjectKeys;
use guardian_db::supabase::{AppState, ServiceConfig, SupabaseCompatProject, build_router};

const TEST_SECRET: &str = "integration-test-jwt-secret-value-0123456789";
const IAT: i64 = 1_700_000_000;

struct Harness {
    app: Router,
    anon: String,
    service: String,
    db: Arc<Database<MemoryStorage>>,
}

async fn harness() -> Harness {
    let db = Arc::new(Database::new(Arc::new(MemoryStorage::new()), "app"));
    let keys = ProjectKeys::from_secret(TEST_SECRET, IAT).unwrap();
    let anon = keys.anon_key.clone();
    let service = keys.service_role_key.clone();
    let project =
        SupabaseCompatProject::shell("app", "http://127.0.0.1:54321", keys, chrono::Utc::now());
    let state = AppState::new(db.clone(), project, ServiceConfig::default());
    let app = build_router(state);
    Harness {
        app,
        anon,
        service,
        db,
    }
}

/// POST a GraphQL request; returns `(status, body)`.
async fn gql(
    app: &Router,
    apikey: &str,
    bearer: Option<&str>,
    query: &str,
    variables: Value,
) -> (StatusCode, Value) {
    let mut body = json!({ "query": query });
    if !variables.is_null() {
        body["variables"] = variables;
    }
    let mut builder = Request::builder()
        .method("POST")
        .uri("/graphql/v1")
        .header("apikey", apikey)
        .header("content-type", "application/json");
    if let Some(b) = bearer {
        builder = builder.header("authorization", format!("Bearer {b}"));
    }
    let req = builder.body(Body::from(body.to_string())).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Assert a 200 response with `data` and no `errors`; return `data`.
fn data_of(status: StatusCode, body: Value) -> Value {
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body.get("errors").is_none(),
        "unexpected GraphQL errors: {body}"
    );
    body["data"].clone()
}

/// Assert a 200 response carrying GraphQL errors; return the first message.
fn error_of(status: StatusCode, body: Value) -> String {
    assert_eq!(status, StatusCode::OK, "body: {body}");
    body["errors"][0]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected errors, got: {body}"))
        .to_string()
}

/// Blog schema: `authors` 1→N `blog_posts` (FK on `author_id`).
async fn seed_blog(db: &Arc<Database<MemoryStorage>>) {
    let mut s = Session::new(db.clone(), "postgres");
    s.execute(
        "CREATE TABLE authors (id int PRIMARY KEY, name text NOT NULL);
         CREATE TABLE blog_posts (
             id int PRIMARY KEY,
             title text,
             views bigint,
             author_id int REFERENCES authors(id),
             created_at timestamptz,
             uid uuid,
             meta jsonb
         );",
    )
    .await
    .unwrap();
    s.execute(
        "INSERT INTO authors VALUES (1, 'ada'), (2, 'brian');
         INSERT INTO blog_posts VALUES
           (1, 'intro',    10, 1, '2024-01-01T00:00:00Z', '00000000-0000-0000-0000-000000000001', '{\"k\":1}'),
           (2, 'middle',   20, 1, '2024-01-02T00:00:00Z', '00000000-0000-0000-0000-000000000002', null),
           (3, 'advanced', 30, 1, '2024-01-03T00:00:00Z', null, null),
           (4, 'guest',    40, 2, '2024-01-04T00:00:00Z', null, null),
           (5, 'draft',  null, null, null, null, null);",
    )
    .await
    .unwrap();
}

// ===========================================================================
// Introspection
// ===========================================================================

#[tokio::test]
async fn introspection_roots_and_table_types() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{
            __schema {
                queryType { name }
                mutationType { name }
                subscriptionType { name }
                types { name kind }
            }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let schema = &data["__schema"];
    assert_eq!(schema["queryType"]["name"], "Query");
    assert_eq!(schema["mutationType"]["name"], "Mutation");
    assert_eq!(schema["subscriptionType"], Value::Null);
    let names: Vec<&str> = schema["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "blog_posts",
        "blog_postsConnection",
        "blog_postsEdge",
        "blog_postsFilter",
        "blog_postsOrderBy",
        "blog_postsInsertInput",
        "blog_postsInsertResponse",
        "blog_postsUpdateInput",
        "blog_postsUpdateResponse",
        "blog_postsDeleteResponse",
        "authors",
        "Node",
        "PageInfo",
        "OrderByDirection",
        "FilterIs",
        "BigInt",
        "UUID",
        "Datetime",
        "JSON",
        "Cursor",
    ] {
        assert!(names.contains(&expected), "missing type {expected}");
    }
}

#[tokio::test]
async fn introspection_graphiql_style_query_with_fragments() {
    let h = harness().await;
    seed_blog(&h.db).await;
    // The shape graphql-js clients (GraphiQL) send: named fragments over
    // __Type / __InputValue with nested TypeRef expansion and directives.
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"
        query IntrospectionQuery {
          __schema {
            queryType { name }
            mutationType { name }
            subscriptionType { name }
            types { ...FullType }
            directives { name description locations args { ...InputValue } }
          }
        }
        fragment FullType on __Type {
          kind name description
          fields(includeDeprecated: true) {
            name description
            args { ...InputValue }
            type { ...TypeRef }
            isDeprecated deprecationReason
          }
          inputFields { ...InputValue }
          interfaces { ...TypeRef }
          enumValues(includeDeprecated: true) { name description isDeprecated deprecationReason }
          possibleTypes { ...TypeRef }
        }
        fragment InputValue on __InputValue {
          name description type { ...TypeRef } defaultValue
        }
        fragment TypeRef on __Type {
          kind name
          ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
        }
        "#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let schema = &data["__schema"];
    let types = schema["types"].as_array().unwrap();

    // The blog_posts object type carries nodeId: ID!, columns with the right
    // scalars, and relationship fields.
    let bp = types
        .iter()
        .find(|t| t["name"] == "blog_posts")
        .expect("blog_posts type");
    assert_eq!(bp["kind"], "OBJECT");
    assert_eq!(bp["interfaces"][0]["name"], "Node");
    let fields = bp["fields"].as_array().unwrap();
    let field = |n: &str| {
        fields
            .iter()
            .find(|f| f["name"] == n)
            .unwrap_or_else(|| panic!("missing field {n}"))
    };
    assert_eq!(field("nodeId")["type"]["kind"], "NON_NULL");
    assert_eq!(field("nodeId")["type"]["ofType"]["name"], "ID");
    assert_eq!(field("views")["type"]["name"], "BigInt");
    assert_eq!(field("uid")["type"]["name"], "UUID");
    assert_eq!(field("created_at")["type"]["name"], "Datetime");
    assert_eq!(field("meta")["type"]["name"], "JSON");
    // FK-derived relationship: child → parent object field named after the
    // referenced table.
    assert_eq!(field("authors")["type"]["name"], "authors");
    // Parent → child collection field with pagination args.
    let authors = types.iter().find(|t| t["name"] == "authors").unwrap();
    let coll = authors["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "blog_postsCollection")
        .expect("child collection field");
    let arg_names: Vec<&str> = coll["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    for a in [
        "first", "last", "before", "after", "offset", "filter", "orderBy",
    ] {
        assert!(arg_names.contains(&a), "missing collection arg {a}");
    }

    // Mutations carry atMost with default "1".
    let mutation = types.iter().find(|t| t["name"] == "Mutation").unwrap();
    let update = mutation["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "updateblog_postsCollection")
        .expect("update mutation");
    let at_most = update["args"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "atMost")
        .unwrap();
    assert_eq!(at_most["defaultValue"], "1");

    // Directives: @skip and @include only.
    let directive_names: Vec<&str> = schema["directives"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert!(directive_names.contains(&"skip"));
    assert!(directive_names.contains(&"include"));
}

#[tokio::test]
async fn introspection_type_lookup() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ __type(name: "blog_postsFilter") { kind inputFields { name } } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["__type"]["kind"], "INPUT_OBJECT");
    let names: Vec<&str> = data["__type"]["inputFields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    for expected in ["id", "title", "views", "and", "or", "not"] {
        assert!(names.contains(&expected), "missing filter field {expected}");
    }
    // JSON columns are not filterable (documented).
    assert!(!names.contains(&"meta"));
}

// ===========================================================================
// Collections: filter, orderBy, pagination, totalCount
// ===========================================================================

#[tokio::test]
async fn collection_filter_and_order_by() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{
            blog_postsCollection(
                filter: { views: { gte: "20" }, title: { neq: "guest" } }
                orderBy: [{ views: DescNullsLast }]
            ) {
                edges { node { title views } }
                totalCount
            }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let edges = data["blog_postsCollection"]["edges"].as_array().unwrap();
    let titles: Vec<&str> = edges
        .iter()
        .filter_map(|e| e["node"]["title"].as_str())
        .collect();
    assert_eq!(titles, vec!["advanced", "middle"]);
    // BigInt renders as a string, like pg_graphql.
    assert_eq!(edges[0]["node"]["views"], "30");
    assert_eq!(data["blog_postsCollection"]["totalCount"], 2);
}

#[tokio::test]
async fn collection_compound_and_or_filters() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{
            blog_postsCollection(filter: {
                or: [
                    { title: { startsWith: "adv" } },
                    { and: [{ views: { lte: "10" } }, { title: { like: "%tro" } }] }
                ]
            }, orderBy: [{ id: AscNullsLast }]) {
                edges { node { title } }
            }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let titles: Vec<&str> = data["blog_postsCollection"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["node"]["title"].as_str())
        .collect();
    assert_eq!(titles, vec!["intro", "advanced"]);
}

#[tokio::test]
async fn collection_is_null_filter() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ blog_postsCollection(filter: { views: { is: NULL } }) {
            edges { node { title } } totalCount } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["blog_postsCollection"]["totalCount"], 1);
    assert_eq!(
        data["blog_postsCollection"]["edges"][0]["node"]["title"],
        "draft"
    );
}

#[tokio::test]
async fn keyset_pagination_walks_three_pages() {
    let h = harness().await;
    seed_blog(&h.db).await;

    let page = |after: Option<String>| {
        let query = r#"query Page($after: Cursor) {
            blog_postsCollection(first: 2, after: $after) {
                edges { cursor node { id title } }
                pageInfo { hasNextPage hasPreviousPage startCursor endCursor }
                totalCount
            }
        }"#;
        let vars = match after {
            Some(c) => json!({ "after": c }),
            None => json!({}),
        };
        gql(&h.app, &h.anon, None, query, vars)
    };

    // Page 1.
    let (status, body) = page(None).await;
    let d1 = data_of(status, body);
    let c1 = &d1["blog_postsCollection"];
    let ids: Vec<i64> = c1["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(c1["pageInfo"]["hasNextPage"], true);
    assert_eq!(c1["pageInfo"]["hasPreviousPage"], false);
    assert_eq!(c1["totalCount"], 5);
    let end1 = c1["pageInfo"]["endCursor"].as_str().unwrap().to_string();
    assert_eq!(c1["edges"][1]["cursor"], end1.as_str());

    // Page 2.
    let (status, body) = page(Some(end1)).await;
    let d2 = data_of(status, body);
    let c2 = &d2["blog_postsCollection"];
    let ids: Vec<i64> = c2["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![3, 4]);
    assert_eq!(c2["pageInfo"]["hasNextPage"], true);
    assert_eq!(c2["pageInfo"]["hasPreviousPage"], true);
    let end2 = c2["pageInfo"]["endCursor"].as_str().unwrap().to_string();

    // Page 3 (final).
    let (status, body) = page(Some(end2)).await;
    let d3 = data_of(status, body);
    let c3 = &d3["blog_postsCollection"];
    let ids: Vec<i64> = c3["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![5]);
    assert_eq!(c3["pageInfo"]["hasNextPage"], false);
    assert_eq!(c3["pageInfo"]["hasPreviousPage"], true);
}

#[tokio::test]
async fn backward_pagination_with_last() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ blog_postsCollection(last: 2) {
            edges { node { id } }
            pageInfo { hasNextPage hasPreviousPage }
        } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let ids: Vec<i64> = data["blog_postsCollection"]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["node"]["id"].as_i64().unwrap())
        .collect();
    // Rows come back in base (ascending PK) order.
    assert_eq!(ids, vec![4, 5]);
    assert_eq!(
        data["blog_postsCollection"]["pageInfo"]["hasPreviousPage"],
        true
    );
}

#[tokio::test]
async fn cursor_with_order_by_is_a_truthful_error() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let cursor = {
        let (status, body) = gql(
            &h.app,
            &h.anon,
            None,
            r#"{ blog_postsCollection(first: 1) { pageInfo { endCursor } } }"#,
            Value::Null,
        )
        .await;
        let data = data_of(status, body);
        data["blog_postsCollection"]["pageInfo"]["endCursor"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        &format!(
            r#"{{ blog_postsCollection(first: 1, after: "{cursor}",
                 orderBy: [{{ title: AscNullsLast }}]) {{ totalCount }} }}"#
        ),
        Value::Null,
    )
    .await;
    let msg = error_of(status, body);
    assert!(
        msg.contains("orderBy") && msg.contains("not supported"),
        "message: {msg}"
    );
}

// ===========================================================================
// node() lookup and nodeId
// ===========================================================================

#[tokio::test]
async fn node_lookup_by_node_id() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ blog_postsCollection(filter: { id: { eq: 2 } }) {
            edges { node { nodeId title } } } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let node_id = data["blog_postsCollection"]["edges"][0]["node"]["nodeId"]
        .as_str()
        .unwrap()
        .to_string();

    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"query Node($id: ID!) {
            node(nodeId: $id) {
                __typename
                nodeId
                ... on blog_posts { title views }
            }
        }"#,
        json!({ "id": node_id }),
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["node"]["__typename"], "blog_posts");
    assert_eq!(data["node"]["title"], "middle");
    assert_eq!(data["node"]["views"], "20");
    assert_eq!(data["node"]["nodeId"], node_id);

    // A syntactically valid nodeId for a missing row resolves to null.
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"query Node($id: ID!) { node(nodeId: $id) { __typename } }"#,
        json!({ "id": base64_of(&json!(["public", "blog_posts", 999]).to_string()) }),
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["node"], Value::Null);
}

fn base64_of(s: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(s)
}

// ===========================================================================
// Relationships
// ===========================================================================

#[tokio::test]
async fn fk_parent_object_and_child_collection() {
    let h = harness().await;
    seed_blog(&h.db).await;

    // Child → parent object field (named after the referenced table).
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ blog_postsCollection(filter: { id: { in: [1, 5] } }) {
            edges { node { title authors { name } } } } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let edges = data["blog_postsCollection"]["edges"].as_array().unwrap();
    assert_eq!(edges[0]["node"]["authors"]["name"], "ada");
    // NULL FK → null parent, not an error.
    assert_eq!(edges[1]["node"]["authors"], Value::Null);

    // Parent → child collection with filter + first.
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ authorsCollection(filter: { id: { eq: 1 } }) {
            edges { node {
                name
                blog_postsCollection(filter: { views: { gt: "10" } }, first: 1) {
                    edges { node { title } }
                    totalCount
                }
            } }
        } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let author = &data["authorsCollection"]["edges"][0]["node"];
    assert_eq!(author["name"], "ada");
    let posts = &author["blog_postsCollection"];
    assert_eq!(posts["edges"][0]["node"]["title"], "middle");
    // totalCount respects the implicit FK restriction and the filter.
    assert_eq!(posts["totalCount"], 2);
}

// ===========================================================================
// Mutations
// ===========================================================================

#[tokio::test]
async fn insert_update_delete_mutations() {
    let h = harness().await;
    seed_blog(&h.db).await;

    // Insert two authors at once.
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation {
            insertIntoauthorsCollection(objects: [
                { id: 10, name: "carol" },
                { id: 11, name: "dave" }
            ]) {
                affectedCount
                records { id name nodeId }
            }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    let resp = &data["insertIntoauthorsCollection"];
    assert_eq!(resp["affectedCount"], 2);
    assert_eq!(resp["records"][0]["name"], "carol");
    assert!(resp["records"][0]["nodeId"].is_string());

    // Update exactly one row.
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation {
            updateauthorsCollection(
                set: { name: "carol2" }
                filter: { id: { eq: 10 } }
                atMost: 1
            ) { affectedCount records { name } }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["updateauthorsCollection"]["affectedCount"], 1);
    assert_eq!(
        data["updateauthorsCollection"]["records"][0]["name"],
        "carol2"
    );

    // Delete one row (atMost defaults to 1).
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation {
            deleteFromauthorsCollection(filter: { id: { eq: 11 } }) {
                affectedCount records { id }
            }
        }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["deleteFromauthorsCollection"]["affectedCount"], 1);
    assert_eq!(data["deleteFromauthorsCollection"]["records"][0]["id"], 11);
}

#[tokio::test]
async fn at_most_violation_rolls_back() {
    let h = harness().await;
    seed_blog(&h.db).await;

    // 4 posts have a title; atMost 1 must fail and change nothing.
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation {
            updateblog_postsCollection(
                set: { title: "clobbered" }
                filter: { title: { is: NOT_NULL } }
                atMost: 1
            ) { affectedCount }
        }"#,
        Value::Null,
    )
    .await;
    let msg = error_of(status, body);
    assert_eq!(msg, "update impacts too many records");

    // Rolled back: nothing was clobbered.
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"{ blog_postsCollection(filter: { title: { eq: "clobbered" } }) { totalCount } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["blog_postsCollection"]["totalCount"], 0);

    // Same for delete.
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation { deleteFromblog_postsCollection(atMost: 2) { affectedCount } }"#,
        Value::Null,
    )
    .await;
    let msg = error_of(status, body);
    assert_eq!(msg, "delete impacts too many records");
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"{ blog_postsCollection { totalCount } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["blog_postsCollection"]["totalCount"], 5);
}

#[tokio::test]
async fn mutations_rejected_over_get() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let query = "mutation { deleteFromblog_postsCollection(atMost: 99) { affectedCount } }";
    let uri = format!(
        "/graphql/v1?query={}",
        query
            .replace(' ', "%20")
            .replace('{', "%7B")
            .replace('}', "%7D")
    );
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("apikey", &h.anon)
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("not allowed over GET"),
        "body: {body}"
    );
}

// ===========================================================================
// RLS through GraphQL
// ===========================================================================

const UID_A: &str = "0b9fbc1e-6a34-4bff-8df5-6b9f7c4e3d21";
const UID_B: &str = "7f3a1d52-9c1b-4e8e-b0a4-2c5d9e8f7a61";

async fn seed_rls_notes(db: &Arc<Database<MemoryStorage>>) {
    let mut s = Session::new(db.clone(), "postgres");
    s.execute("CREATE TABLE notes (id int PRIMARY KEY, user_id text, body text)")
        .await
        .unwrap();
    s.execute(&format!(
        "INSERT INTO notes VALUES (1, '{UID_A}', 'a note'), (2, '{UID_B}', 'b note')"
    ))
    .await
    .unwrap();
    s.execute("ALTER TABLE notes ENABLE ROW LEVEL SECURITY")
        .await
        .unwrap();
    s.execute(
        "CREATE POLICY notes_select ON notes FOR SELECT TO authenticated \
         USING (user_id = auth.uid()::text)",
    )
    .await
    .unwrap();
}

fn user_token(sub: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let mut claims = guardian_db::supabase::Claims::api_key("authenticated", now, now + 3600);
    claims.sub = Some(sub.to_string());
    claims.aud = Some("authenticated".to_string());
    guardian_db::supabase::jwt::sign(&claims, TEST_SECRET).unwrap()
}

#[tokio::test]
async fn rls_governs_graphql_collections() {
    let h = harness().await;
    seed_rls_notes(&h.db).await;
    let query = r#"{ notesCollection { edges { node { id user_id } } totalCount } }"#;

    // anon: policies target `authenticated` → default deny, zero rows.
    let (status, body) = gql(&h.app, &h.anon, None, query, Value::Null).await;
    let data = data_of(status, body);
    assert_eq!(data["notesCollection"]["totalCount"], 0);
    assert_eq!(data["notesCollection"]["edges"], json!([]));

    // authenticated user A: exactly their row (filter and totalCount agree).
    let token = user_token(UID_A);
    let (status, body) = gql(&h.app, &h.anon, Some(&token), query, Value::Null).await;
    let data = data_of(status, body);
    assert_eq!(data["notesCollection"]["totalCount"], 1);
    assert_eq!(
        data["notesCollection"]["edges"][0]["node"]["user_id"],
        UID_A
    );

    // service_role bypasses policies.
    let (status, body) = gql(&h.app, &h.service, None, query, Value::Null).await;
    let data = data_of(status, body);
    assert_eq!(data["notesCollection"]["totalCount"], 2);
}

// ===========================================================================
// Document features: variables, fragments, aliases, @skip / @include
// ===========================================================================

#[tokio::test]
async fn variables_fragments_aliases_and_skip() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"
        query Posts($minViews: BigIntFilter, $withViews: Boolean!, $n: Int = 2) {
            firstPosts: blog_postsCollection(first: $n, filter: { views: $minViews }) {
                edges { node { ...PostBits } }
            }
        }
        fragment PostBits on blog_posts {
            headline: title
            views @include(if: $withViews)
            id @skip(if: $withViews)
        }
        "#,
        json!({ "minViews": { "gte": "30" }, "withViews": true }),
    )
    .await;
    let data = data_of(status, body);
    let edges = data["firstPosts"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    let node = &edges[0]["node"];
    assert_eq!(node["headline"], "advanced");
    assert_eq!(node["views"], "30");
    // @skip'd field is absent, not null.
    assert!(node.get("id").is_none());
    // The un-aliased name is absent when aliased.
    assert!(node.get("title").is_none());
}

#[tokio::test]
async fn missing_required_variable_is_an_error() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"query Q($flag: Boolean!) { blog_postsCollection { edges { node { id @skip(if: $flag) } } } }"#,
        json!({}),
    )
    .await;
    let msg = error_of(status, body);
    assert!(msg.contains("$flag"), "message: {msg}");
}

// ===========================================================================
// Error shapes
// ===========================================================================

#[tokio::test]
async fn unknown_field_is_a_graphql_error() {
    let h = harness().await;
    seed_blog(&h.db).await;
    let (status, body) = gql(&h.app, &h.anon, None, "{ nope }", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], Value::Null);
    let msg = body["errors"][0]["message"].as_str().unwrap();
    assert!(
        msg.contains("Unknown field") && msg.contains("nope"),
        "message: {msg}"
    );

    // Unknown column inside a selection.
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        "{ blog_postsCollection { edges { node { no_such_column } } } }",
        Value::Null,
    )
    .await;
    let msg = error_of(status, body);
    assert!(msg.contains("no_such_column"), "message: {msg}");
}

#[tokio::test]
async fn subscriptions_are_a_truthful_error() {
    let h = harness().await;
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        "subscription { anything }",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msg = body["errors"][0]["message"].as_str().unwrap();
    assert!(msg.contains("subscriptions are not supported"), "{msg}");
}

#[tokio::test]
async fn syntax_error_is_a_graphql_error() {
    let h = harness().await;
    let (status, body) = gql(&h.app, &h.anon, None, "{ unbalanced", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("syntax error"),
        "body: {body}"
    );
}

#[tokio::test]
async fn graphql_requires_apikey() {
    let h = harness().await;
    let req = Request::builder()
        .method("POST")
        .uri("/graphql/v1")
        .header("content-type", "application/json")
        .body(Body::from(json!({"query": "{ __typename }"}).to_string()))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "SUPA_COMPAT_MISSING_API_KEY");
}

#[tokio::test]
async fn graphql_subpath_is_typed_not_bare_404() {
    let h = harness().await;
    let req = Request::builder()
        .method("POST")
        .uri("/graphql/v1/extra")
        .header("apikey", &h.anon)
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["errors"][0]["message"].is_string(), "body: {body}");
}

// ===========================================================================
// Scalar round-trips
// ===========================================================================

#[tokio::test]
async fn bigint_uuid_datetime_scalars_round_trip() {
    let h = harness().await;
    seed_blog(&h.db).await;

    // Insert through GraphQL with variables: BigInt beyond Int32 as a string,
    // a UUID string, an ISO timestamp, and a JSON string. (The engine's
    // insert coercion routes integers through f64, so values beyond 2^53
    // lose precision engine-wide — REST and GraphQL alike; documented.)
    let (status, body) = gql(
        &h.app,
        &h.service,
        None,
        r#"mutation Ins($objects: [blog_postsInsertInput!]!) {
            insertIntoblog_postsCollection(objects: $objects) {
                affectedCount
                records { id views uid created_at meta }
            }
        }"#,
        json!({ "objects": [{
            "id": 100,
            "title": "scalars",
            "views": "4503599627370497",
            "uid": "123e4567-e89b-12d3-a456-426614174000",
            "created_at": "2024-06-15T12:30:45Z",
            "meta": "{\"nested\":{\"a\":[1,2]}}"
        }] }),
    )
    .await;
    let data = data_of(status, body);
    let rec = &data["insertIntoblog_postsCollection"]["records"][0];
    assert_eq!(rec["views"], "4503599627370497"); // BigInt: exact, as string
    assert_eq!(rec["uid"], "123e4567-e89b-12d3-a456-426614174000");
    assert!(
        rec["created_at"]
            .as_str()
            .unwrap()
            .starts_with("2024-06-15T12:30:45"),
        "created_at: {}",
        rec["created_at"]
    );
    // JSON scalar: serialized string, opaque.
    let meta: Value = serde_json::from_str(rec["meta"].as_str().unwrap()).unwrap();
    assert_eq!(meta["nested"]["a"], json!([1, 2]));

    // Filter by UUID equality and BigInt equality round-trips too.
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        r#"{ blog_postsCollection(filter: {
            uid: { eq: "123e4567-e89b-12d3-a456-426614174000" }
            views: { eq: "4503599627370497" }
        }) { edges { node { id } } } }"#,
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["blog_postsCollection"]["edges"][0]["node"]["id"], 100);
}

// ===========================================================================
// Reflection rules
// ===========================================================================

#[tokio::test]
async fn table_without_primary_key_is_not_reflected() {
    let h = harness().await;
    {
        let mut s = Session::new(h.db.clone(), "postgres");
        s.execute("CREATE TABLE no_pk (x int)").await.unwrap();
        s.execute("CREATE TABLE with_pk (id int PRIMARY KEY)")
            .await
            .unwrap();
    }
    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        "{ no_pkCollection { totalCount } }",
        Value::Null,
    )
    .await;
    let msg = error_of(status, body);
    assert!(msg.contains("Unknown field"), "message: {msg}");

    let (status, body) = gql(
        &h.app,
        &h.anon,
        None,
        "{ with_pkCollection { totalCount } }",
        Value::Null,
    )
    .await;
    let data = data_of(status, body);
    assert_eq!(data["with_pkCollection"]["totalCount"], 0);
}
