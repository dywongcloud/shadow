Lagon is currently deployed in 14 regions all around the world. We increase the number of regions regularly, and you can also [request a new Region](#request-a-new-region).

We use multiple hosting providers to prevent a single point of failure and provide resiliency in the case of an outage from one of our providers. Users are routed to the region that is geographically closest to them when they request a Function, using a mix of Anycast and GeoIP.

You can access the Region that is responding to the current requests using the [`X-Lagon-Region` header](/runtime-apis#additional-headers).

## List of regions

- Ashburn, Virginia (`ashburn-us-east`)
- Hillsboro, Oregon (`hillsboro-us-west`)
- San Francisco, California (`san-francisco-us-west`)
- Beauharnois, Canada (`beauharnois-ca-east`)
- London, United Kingdon (`london-eu-west`)
- Paris, France (`paris-eu-west`)
- Nuremberg, Germany (`nuremberg-eu-central`)
- Helsinki, Finland (`helsinki-eu-north`)
- Warsaw, Poland (`warsaw-eu-east`)
- Bangalore, India (`bangalore-ap-west`)
- Singapore (`singapore-ap-south`)
- Sydney, Australia (`sydney-ap-south`)
- Tokio, Japan (`tokio-ap-east`)
- Johannesburg, South Africa (`johannesburg-af-south`)

## Request a new region

If you would like to see a new region, please [fill out this form](https://tally.so/r/mDqAYN). We will prioritize the regions that are the most requested.

A plan belongs to an Organization. Additionally, [limits](/cloud/limits) apply to each plan.

## Personal

Perfect for side projects and personal use, this plan is **free** and includes 3,000,000 requests per month across all Functions in your Organization. A Personal Organization can only have an owner, without any members.

You also get access to the following features:

- 5s of execution per request
- Preview and Production deployments
- Automatic HTTPS
- Custom domains
- Logs and analytics

## Pro

Made for startups and small teams, this plan starts at **$10/month** and includes 5,000,000 requests per month across all Functions in your Organization. Additional requests are billed at $1 per 1,000,000 requests.

You get access to all of the features in the Personal plan, plus:

- 30s of execution per request
- Organization members
- Up to 50 Functions
- Up to 1000 assets per Deployment

## Enterprise

This plan is designed for customers that require custom limits and/or need a large volume of requests. [Contact us](mailto:contact@lagon.app) to learn more.
