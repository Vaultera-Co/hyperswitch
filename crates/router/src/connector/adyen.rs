mod transformers;

use std::fmt::Debug;

use bytes::Bytes;
use error_stack::{IntoReport, ResultExt};
use router_env::{tracing, tracing::instrument};

use self::transformers as adyen;
use crate::{
    configs::settings,
    core::{
        errors::{self, CustomResult},
        payments,
    },
    db::StorageInterface,
    headers, logger, services,
    types::{
        self,
        api::{self, ConnectorCommon},
    },
    utils::{self, crypto, ByteSliceExt, BytesExt, OptionExt},
};

#[derive(Debug, Clone)]
pub struct Adyen;

impl api::ConnectorCommon for Adyen {
    fn id(&self) -> &'static str {
        "adyen"
    }

    fn get_auth_header(
        &self,
        auth_type: &types::ConnectorAuthType,
    ) -> CustomResult<Vec<(String, String)>, errors::ConnectorError> {
        let auth: adyen::AdyenAuthType = auth_type
            .try_into()
            .change_context(errors::ConnectorError::FailedToObtainAuthType)?;
        Ok(vec![(headers::X_API_KEY.to_string(), auth.api_key)])
    }

    fn base_url<'a>(&self, connectors: &'a settings::Connectors) -> &'a str {
        connectors.adyen.base_url.as_ref()
    }
}

impl api::Payment for Adyen {}
impl api::PaymentAuthorize for Adyen {}
impl api::PaymentSync for Adyen {}
impl api::PaymentVoid for Adyen {}
impl api::PaymentCapture for Adyen {}
impl api::PreVerify for Adyen {}

impl
    services::ConnectorIntegration<
        api::Verify,
        types::VerifyRequestData,
        types::PaymentsResponseData,
    > for Adyen
{
    // Issue: #173
}

impl api::PaymentSession for Adyen {}

impl
    services::ConnectorIntegration<
        api::Session,
        types::PaymentsSessionData,
        types::PaymentsResponseData,
    > for Adyen
{
    // Not Implemented (R)
}

impl
    services::ConnectorIntegration<
        api::Capture,
        types::PaymentsCaptureData,
        types::PaymentsResponseData,
    > for Adyen
{
    // Not Implemented (R)
}

impl
    services::ConnectorIntegration<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>
    for Adyen
{
    fn get_headers(
        &self,
        req: &types::RouterData<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>,
    ) -> CustomResult<Vec<(String, String)>, errors::ConnectorError> {
        let mut header = vec![
            (
                headers::CONTENT_TYPE.to_string(),
                types::PaymentsSyncType::get_content_type(self).to_string(),
            ),
            (headers::X_ROUTER.to_string(), "test".to_string()),
        ];
        let mut api_key = self.get_auth_header(&req.connector_auth_type)?;
        header.append(&mut api_key);
        Ok(header)
    }

    fn get_request_body(
        &self,
        req: &types::RouterData<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>,
    ) -> CustomResult<Option<String>, errors::ConnectorError> {
        let encoded_data = req
            .request
            .encoded_data
            .clone()
            .get_required_value("encoded_data")
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;

        let adyen_redirection_type = serde_urlencoded::from_str::<
            transformers::AdyenRedirectRequestTypes,
        >(encoded_data.as_str())
        .into_report()
        .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        let redirection_request = match adyen_redirection_type {
            adyen::AdyenRedirectRequestTypes::AdyenRedirection(req) => {
                adyen::AdyenRedirectRequest {
                    details: adyen::AdyenRedirectRequestTypes::AdyenRedirection(
                        adyen::AdyenRedirection {
                            redirect_result: req.redirect_result,
                            type_of_redirection_result: None,
                            result_code: None,
                        },
                    ),
                }
            }
            adyen::AdyenRedirectRequestTypes::AdyenThreeDS(req) => adyen::AdyenRedirectRequest {
                details: adyen::AdyenRedirectRequestTypes::AdyenThreeDS(adyen::AdyenThreeDS {
                    three_ds_result: req.three_ds_result,
                    type_of_redirection_result: None,
                    result_code: None,
                }),
            },
        };

        let adyen_request = utils::Encode::<adyen::AdyenRedirectRequest>::encode_to_string_of_json(
            &redirection_request,
        )
        .change_context(errors::ConnectorError::RequestEncodingFailed)?;

        Ok(Some(adyen_request))
    }

    fn get_url(
        &self,
        _req: &types::RouterData<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!(
            "{}{}",
            self.base_url(connectors),
            "v68/payments/details"
        ))
    }

    fn build_request(
        &self,
        req: &types::RouterData<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        Ok(Some(
            services::RequestBuilder::new()
                .method(services::Method::Post)
                .url(&types::PaymentsSyncType::get_url(self, req, connectors)?)
                .headers(types::PaymentsSyncType::get_headers(self, req)?)
                .header(headers::X_ROUTER, "test")
                .body(types::PaymentsSyncType::get_request_body(self, req)?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &types::RouterData<api::PSync, types::PaymentsSyncData, types::PaymentsResponseData>,
        res: types::Response,
    ) -> CustomResult<types::PaymentsSyncRouterData, errors::ConnectorError> {
        logger::debug!(payment_sync_response=?res);
        let response: adyen::AdyenPaymentResponse = res
            .response
            .parse_struct("AdyenPaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: Bytes,
    ) -> CustomResult<types::ErrorResponse, errors::ConnectorError> {
        let response: adyen::ErrorResponse = res
            .parse_struct("ErrorResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        Ok(types::ErrorResponse {
            code: response.error_code,
            message: response.message,
            reason: None,
        })
    }
}

impl
    services::ConnectorIntegration<
        api::Authorize,
        types::PaymentsAuthorizeData,
        types::PaymentsResponseData,
    > for Adyen
{
    fn get_headers(
        &self,
        req: &types::PaymentsAuthorizeRouterData,
    ) -> CustomResult<Vec<(String, String)>, errors::ConnectorError>
    where
        Self: services::ConnectorIntegration<
            api::Authorize,
            types::PaymentsAuthorizeData,
            types::PaymentsResponseData,
        >,
    {
        let mut header = vec![
            (
                headers::CONTENT_TYPE.to_string(),
                types::PaymentsAuthorizeType::get_content_type(self).to_string(),
            ),
            (headers::X_ROUTER.to_string(), "test".to_string()),
        ];
        let mut api_key = self.get_auth_header(&req.connector_auth_type)?;
        header.append(&mut api_key);
        Ok(header)
    }

    fn get_url(
        &self,
        _req: &types::PaymentsAuthorizeRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}{}", self.base_url(connectors), "v68/payments"))
    }

    fn get_request_body(
        &self,
        req: &types::PaymentsAuthorizeRouterData,
    ) -> CustomResult<Option<String>, errors::ConnectorError> {
        let adyen_req = utils::Encode::<adyen::AdyenPaymentRequest>::convert_and_encode(req)
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;
        Ok(Some(adyen_req))
    }

    fn build_request(
        &self,
        req: &types::RouterData<
            api::Authorize,
            types::PaymentsAuthorizeData,
            types::PaymentsResponseData,
        >,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        Ok(Some(
            services::RequestBuilder::new()
                .method(services::Method::Post)
                .url(&types::PaymentsAuthorizeType::get_url(
                    self, req, connectors,
                )?)
                .headers(types::PaymentsAuthorizeType::get_headers(self, req)?)
                .header(headers::X_ROUTER, "test")
                .body(types::PaymentsAuthorizeType::get_request_body(self, req)?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &types::PaymentsAuthorizeRouterData,
        res: types::Response,
    ) -> CustomResult<types::PaymentsAuthorizeRouterData, errors::ConnectorError> {
        let response: adyen::AdyenPaymentResponse = res
            .response
            .parse_struct("AdyenPaymentResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;

        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: Bytes,
    ) -> CustomResult<types::ErrorResponse, errors::ConnectorError> {
        let response: adyen::ErrorResponse = res
            .parse_struct("ErrorResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        Ok(types::ErrorResponse {
            code: response.error_code,
            message: response.message,
            reason: None,
        })
    }
}

impl
    services::ConnectorIntegration<
        api::Void,
        types::PaymentsCancelData,
        types::PaymentsResponseData,
    > for Adyen
{
    fn get_headers(
        &self,
        req: &types::PaymentsCancelRouterData,
    ) -> CustomResult<Vec<(String, String)>, errors::ConnectorError> {
        let mut header = vec![
            (
                headers::CONTENT_TYPE.to_string(),
                types::PaymentsAuthorizeType::get_content_type(self).to_string(),
            ),
            (headers::X_ROUTER.to_string(), "test".to_string()),
        ];
        let mut api_key = self.get_auth_header(&req.connector_auth_type)?;
        header.append(&mut api_key);
        Ok(header)
    }

    fn get_url(
        &self,
        _req: &types::PaymentsCancelRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        Ok(format!("{}{}", self.base_url(connectors), "v68/cancel"))
    }

    fn get_request_body(
        &self,
        req: &types::PaymentsCancelRouterData,
    ) -> CustomResult<Option<String>, errors::ConnectorError> {
        let adyen_req = utils::Encode::<adyen::AdyenCancelRequest>::convert_and_encode(req)
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;
        Ok(Some(adyen_req))
    }
    fn build_request(
        &self,
        req: &types::PaymentsCancelRouterData,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        Ok(Some(
            services::RequestBuilder::new()
                .method(services::Method::Post)
                .url(&types::PaymentsVoidType::get_url(self, req, connectors)?)
                .headers(types::PaymentsVoidType::get_headers(self, req)?)
                .header(headers::X_ROUTER, "test")
                .body(types::PaymentsVoidType::get_request_body(self, req)?)
                .build(),
        ))
    }

    fn handle_response(
        &self,
        data: &types::PaymentsCancelRouterData,
        res: types::Response,
    ) -> CustomResult<types::PaymentsCancelRouterData, errors::ConnectorError> {
        let response: adyen::AdyenCancelResponse = res
            .response
            .parse_struct("AdyenCancelResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: Bytes,
    ) -> CustomResult<types::ErrorResponse, errors::ConnectorError> {
        let response: adyen::ErrorResponse = res
            .parse_struct("ErrorResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        logger::info!(response=?res);
        Ok(types::ErrorResponse {
            code: response.error_code,
            message: response.message,
            reason: None,
        })
    }
}

impl api::Refund for Adyen {}
impl api::RefundExecute for Adyen {}
impl api::RefundSync for Adyen {}

impl services::ConnectorIntegration<api::Execute, types::RefundsData, types::RefundsResponseData>
    for Adyen
{
    fn get_headers(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
    ) -> CustomResult<Vec<(String, String)>, errors::ConnectorError> {
        let mut header = vec![
            (
                headers::CONTENT_TYPE.to_string(),
                types::RefundExecuteType::get_content_type(self).to_string(),
            ),
            (headers::X_ROUTER.to_string(), "test".to_string()),
        ];
        let mut api_header = self.get_auth_header(&req.connector_auth_type)?;
        header.append(&mut api_header);
        Ok(header)
    }

    fn get_url(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
        connectors: &settings::Connectors,
    ) -> CustomResult<String, errors::ConnectorError> {
        let connector_payment_id = req.request.connector_transaction_id.clone();
        Ok(format!(
            "{}v68/payments/{}/reversals",
            self.base_url(connectors),
            connector_payment_id,
        ))
    }

    fn get_request_body(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
    ) -> CustomResult<Option<String>, errors::ConnectorError> {
        let adyen_req = utils::Encode::<adyen::AdyenRefundRequest>::convert_and_encode(req)
            .change_context(errors::ConnectorError::RequestEncodingFailed)?;
        Ok(Some(adyen_req))
    }

    fn build_request(
        &self,
        req: &types::RefundsRouterData<api::Execute>,
        connectors: &settings::Connectors,
    ) -> CustomResult<Option<services::Request>, errors::ConnectorError> {
        Ok(Some(
            services::RequestBuilder::new()
                .method(services::Method::Post)
                .url(&types::RefundExecuteType::get_url(self, req, connectors)?)
                .headers(types::RefundExecuteType::get_headers(self, req)?)
                .body(types::RefundExecuteType::get_request_body(self, req)?)
                .build(),
        ))
    }

    #[instrument(skip_all)]
    fn handle_response(
        &self,
        data: &types::RefundsRouterData<api::Execute>,
        res: types::Response,
    ) -> CustomResult<types::RefundsRouterData<api::Execute>, errors::ConnectorError> {
        let response: adyen::AdyenRefundResponse = res
            .response
            .parse_struct("AdyenRefundResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        logger::info!(response=?res);
        types::RouterData::try_from(types::ResponseRouterData {
            response,
            data: data.clone(),
            http_code: res.status_code,
        })
        .change_context(errors::ConnectorError::ResponseHandlingFailed)
    }

    fn get_error_response(
        &self,
        res: Bytes,
    ) -> CustomResult<types::ErrorResponse, errors::ConnectorError> {
        let response: adyen::ErrorResponse = res
            .parse_struct("ErrorResponse")
            .change_context(errors::ConnectorError::ResponseDeserializationFailed)?;
        logger::info!(response=?res);
        Ok(types::ErrorResponse {
            code: response.error_code,
            message: response.message,
            reason: None,
        })
    }
}

impl services::ConnectorIntegration<api::RSync, types::RefundsData, types::RefundsResponseData>
    for Adyen
{
}

fn get_webhook_object_from_body(
    body: &[u8],
) -> CustomResult<adyen::AdyenNotificationRequestItemWH, errors::ParsingError> {
    let mut webhook: adyen::AdyenIncomingWebhook = body.parse_struct("AdyenIncomingWebhook")?;

    let item_object = webhook
        .notification_items
        .drain(..)
        .next()
        .ok_or(errors::ParsingError)
        .into_report()?;

    Ok(item_object.notification_request_item)
}

#[async_trait::async_trait]
impl api::IncomingWebhook for Adyen {
    fn get_webhook_source_verification_algorithm(
        &self,
        _headers: &actix_web::http::header::HeaderMap,
        _body: &[u8],
    ) -> CustomResult<Box<dyn crypto::VerifySignature + Send>, errors::ConnectorError> {
        Ok(Box::new(crypto::HmacSha256))
    }

    fn get_webhook_source_verification_signature(
        &self,
        _headers: &actix_web::http::header::HeaderMap,
        body: &[u8],
    ) -> CustomResult<Vec<u8>, errors::ConnectorError> {
        let notif_item = get_webhook_object_from_body(body)
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;

        let base64_signature = notif_item.additional_data.hmac_signature;

        let signature = base64::decode(base64_signature.as_bytes())
            .into_report()
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;

        Ok(signature)
    }

    fn get_webhook_source_verification_message(
        &self,
        _headers: &actix_web::http::header::HeaderMap,
        body: &[u8],
    ) -> CustomResult<Vec<u8>, errors::ConnectorError> {
        let notif = get_webhook_object_from_body(body)
            .change_context(errors::ConnectorError::WebhookSourceVerificationFailed)?;

        let message = format!(
            "{}:{}:{}:{}:{}:{}:{}:{}",
            notif.psp_reference,
            notif.original_reference.unwrap_or_default(),
            notif.merchant_account_code,
            notif.merchant_reference,
            notif.amount.value,
            notif.amount.currency,
            notif.event_code,
            notif.success
        );

        Ok(message.into_bytes())
    }

    async fn get_webhook_source_verification_merchant_secret(
        &self,
        db: &dyn StorageInterface,
        merchant_id: &str,
    ) -> CustomResult<Vec<u8>, errors::ConnectorError> {
        let key = format!("whsec_verification_{}_{}", self.id(), merchant_id);
        let secret = db
            .get_key(&key)
            .await
            .change_context(errors::ConnectorError::WebhookVerificationSecretNotFound)?;

        Ok(secret)
    }

    fn get_webhook_object_reference_id(
        &self,
        body: &[u8],
    ) -> CustomResult<String, errors::ConnectorError> {
        let notif = get_webhook_object_from_body(body)
            .change_context(errors::ConnectorError::WebhookReferenceIdNotFound)?;

        Ok(notif.psp_reference)
    }

    fn get_webhook_event_type(
        &self,
        body: &[u8],
    ) -> CustomResult<api::IncomingWebhookEvent, errors::ConnectorError> {
        let notif = get_webhook_object_from_body(body)
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;

        Ok(match notif.event_code.as_str() {
            "AUTHORISATION" => api::IncomingWebhookEvent::PaymentIntentSuccess,
            _ => Err(errors::ConnectorError::WebhookEventTypeNotFound).into_report()?,
        })
    }

    fn get_webhook_resource_object(
        &self,
        body: &[u8],
    ) -> CustomResult<serde_json::Value, errors::ConnectorError> {
        let notif = get_webhook_object_from_body(body)
            .change_context(errors::ConnectorError::WebhookEventTypeNotFound)?;

        let response: adyen::AdyenResponse = notif.into();

        let res_json = serde_json::to_value(response)
            .into_report()
            .change_context(errors::ConnectorError::WebhookResourceObjectNotFound)?;

        Ok(res_json)
    }

    fn get_webhook_api_response(
        &self,
    ) -> CustomResult<services::api::BachResponse<serde_json::Value>, errors::ConnectorError> {
        Ok(services::api::BachResponse::TextPlain(
            "[accepted]".to_string(),
        ))
    }
}

impl services::ConnectorRedirectResponse for Adyen {
    fn get_flow_type(
        &self,
        _query_params: &str,
    ) -> CustomResult<payments::CallConnectorAction, errors::ConnectorError> {
        Ok(payments::CallConnectorAction::Trigger)
    }
}
