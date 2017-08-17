#![allow(warnings)]

use std::marker::PhantomData;

use hyper::Method;

use handler::NewHandler;
use router::Router;
use router::tree::TreeBuilder;
use router::response::finalizer::ResponseFinalizerBuilder;
use router::route::{Delegation, Extractors, RouteImpl};
use router::route::matcher::{RouteMatcher, MethodOnlyRouteMatcher};
use router::route::dispatch::{PipelineHandleChain, PipelineSet, DispatcherImpl};
use router::request::path::{PathExtractor, NoopPathExtractor};
use router::request::query_string::{QueryStringExtractor, NoopQueryStringExtractor};
use router::tree::node::{SegmentType, NodeBuilder};

/// ```rust
/// let pipelines = new_pipeline_set();
/// let (pipelines, default) = pipelines.add(
///     new_pipeline()
///         .add(NewSessionMiddleware::default())
///         .build()
/// );
///
/// router(pipelines, default, |route| {
///     route.get("/").to(welcome::index);
/// })
/// ```
pub fn build_router<C, P, F>(pipeline_chain: C, pipelines: PipelineSet<P>, f: F) -> Router
where
    C: PipelineHandleChain<P>,
    F: FnOnce(&mut RouterBuilder<C, P>),
{
    let mut tree_builder = TreeBuilder::new();

    let response_finalizer = {
        let mut builder = RouterBuilder {
            node_builder: tree_builder.borrow_root_mut(),
            pipeline_chain,
            pipelines,
            response_finalizer_builder: ResponseFinalizerBuilder::new(),
        };

        f(&mut builder);

        builder.response_finalizer_builder.finalize()
    };

    Router::new(tree_builder.finalize(), response_finalizer)
}

pub struct RouterBuilder<'a, C, P> {
    node_builder: &'a mut NodeBuilder,
    pipeline_chain: C,
    pipelines: PipelineSet<P>,
    response_finalizer_builder: ResponseFinalizerBuilder,
}

type DefaultRouterBuilderTo<'a, C, P> = RouterBuilderTo<
    'a,
    MethodOnlyRouteMatcher,
    C,
    P,
    NoopPathExtractor,
    NoopQueryStringExtractor,
>;

impl<'a, C, P> RouterBuilder<'a, C, P>
where
    C: PipelineHandleChain<P> + Copy,
{
    pub fn get<'b>(&'b mut self, path: &str) -> DefaultRouterBuilderTo<'b, C, P>
    where
        C: PipelineHandleChain<P> + Send + Sync + 'static,
        P: Send + Sync + 'static,
    {
        self.request(vec![Method::Get, Method::Head], path)
    }

    pub fn post<'b>(&'b mut self, path: &str) -> DefaultRouterBuilderTo<'b, C, P>
    where
        C: PipelineHandleChain<P> + Send + Sync + 'static,
        P: Send + Sync + 'static,
    {
        self.request(vec![Method::Post], path)
    }

    pub fn request<'b>(
        &'b mut self,
        methods: Vec<Method>,
        path: &str,
    ) -> DefaultRouterBuilderTo<'b, C, P>
    where
        C: PipelineHandleChain<P> + Send + Sync + 'static,
        P: Send + Sync + 'static,
    {
        let path = if path.starts_with("/") {
            &path[1..]
        } else {
            path
        };

        let node_builder = if path.is_empty() {
            &mut self.node_builder
        } else {
            build_subtree(self.node_builder, path.split("/"))
        };

        let matcher = MethodOnlyRouteMatcher::new(methods);

        RouterBuilderTo {
            matcher,
            node_builder,
            pipeline_chain: self.pipeline_chain,
            pipelines: self.pipelines.clone(),
            delegation: Delegation::Internal,
            phantom: PhantomData,
        }
    }
}

pub struct RouterBuilderTo<'a, M, C, P, PE, QSE>
where
    M: RouteMatcher + Send + Sync + 'static,
    C: PipelineHandleChain<P> + Send + Sync + 'static,
    P: Send + Sync + 'static,
    PE: PathExtractor + Send + Sync + 'static,
    QSE: QueryStringExtractor + Send + Sync + 'static,
{
    node_builder: &'a mut NodeBuilder,
    matcher: M,
    pipeline_chain: C,
    pipelines: PipelineSet<P>,
    delegation: Delegation,
    phantom: PhantomData<(PE, QSE)>,
}

impl<'a, M, C, P, PE, QSE> RouterBuilderTo<'a, M, C, P, PE, QSE>
where
    M: RouteMatcher + Send + Sync + 'static,
    C: PipelineHandleChain<P> + Send + Sync + 'static,
    P: Send + Sync + 'static,
    PE: PathExtractor + Send + Sync + 'static,
    QSE: QueryStringExtractor + Send + Sync + 'static,
{
    pub fn to<NH>(self, new_handler: NH)
    where
        NH: NewHandler + Send + Sync + 'static,
    {
        let dispatcher = DispatcherImpl::new(new_handler, self.pipeline_chain, self.pipelines);
        let route: RouteImpl<M, PE, QSE> = RouteImpl::new(
            self.matcher,
            Box::new(dispatcher),
            Extractors::new(),
            self.delegation,
        );
        self.node_builder.add_route(Box::new(route));
    }
}

fn build_subtree<'n, 's, I>(node: &'n mut NodeBuilder, mut i: I) -> &'n mut NodeBuilder
where
    I: Iterator<Item = &'s str>,
{
    match i.next() {
        Some(segment) => {
            println!("router::builder::build_subtree descending into {}", segment);
            let (segment, segment_type) = if segment.starts_with(":") {
                (&segment[1..], SegmentType::Dynamic)
            } else {
                (segment, SegmentType::Static)
            };

            if !node.has_child(segment, segment_type.clone()) {
                let node_builder = NodeBuilder::new(segment, segment_type.clone());
                node.add_child(node_builder);
            }

            let child = node.borrow_mut_child(segment, segment_type).unwrap();
            build_subtree(child, i)
        }
        None => {
            println!("router::builder::build_subtree reached node");
            node
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use hyper::{Request, Response, StatusCode, Method};
    use hyper::server::{NewService, Service};
    use futures::Future;

    use middleware::pipeline::new_pipeline;
    use middleware::session::NewSessionMiddleware;
    use state::State;
    use handler::{Handler, NewHandlerService};
    use router::route::dispatch::{new_pipeline_set, finalize_pipeline_set};

    mod welcome {
        use super::*;
        pub fn index(state: State, req: Request) -> (State, Response) {
            (state, Response::new().with_status(StatusCode::Ok))
        }
    }

    mod api {
        use super::*;
        pub fn submit(state: State, req: Request) -> (State, Response) {
            (state, Response::new().with_status(StatusCode::Accepted))
        }
    }

    #[test]
    fn build_router_test() {
        let pipelines = new_pipeline_set();
        let (pipelines, default) =
            pipelines.add(new_pipeline().add(NewSessionMiddleware::default()).build());

        let pipelines = finalize_pipeline_set(pipelines);

        let default_pipeline_chain = (default, ());

        let router = build_router(default_pipeline_chain, pipelines, |route| {
            route.get("/").to(|| Ok(welcome::index));
            route.post("/api/submit").to(|| Ok(api::submit));
        });

        let new_service = NewHandlerService::new(router);

        let service = new_service.new_service().unwrap();

        let response = service
            .call(Request::new(Method::Get, "/".parse().unwrap()))
            .wait()
            .unwrap();

        assert_eq!(response.status(), StatusCode::Ok);

        let service = new_service.new_service().unwrap();

        let response = service
            .call(Request::new(Method::Post, "/api/submit".parse().unwrap()))
            .wait()
            .unwrap();

        assert_eq!(response.status(), StatusCode::Accepted);
    }
}