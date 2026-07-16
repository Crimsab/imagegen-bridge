# API explorer

Browse the checked OpenAPI 3.1 contract published with Imagegen Bridge. Use the
filter to find an operation, then expand it to inspect parameters, request
bodies, responses, and schemas.

!!! info "Reference only"
    This explorer is intentionally read-only. Imagegen Bridge runs on your own
    infrastructure, so there is no shared public server against which requests
    could be executed. To call your deployment, follow the [HTTP API guide](api.md)
    and send its bearer token to your own bridge URL.

The explorer reads the [version-controlled OpenAPI document](https://github.com/Crimsab/imagegen-bridge/blob/main/schemas/imagegen-bridge-v1.openapi.json)
from the repository. The same contract is available from a running bridge at
`GET /v1/openapi.json`.

<swagger-ui src="https://raw.githubusercontent.com/Crimsab/imagegen-bridge/main/schemas/imagegen-bridge-v1.openapi.json"/>
